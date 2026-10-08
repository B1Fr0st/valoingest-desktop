//! Persistent settings and the upload ledger, stored as JSON under
//! `%LOCALAPPDATA%\Valolysis`. Writes are atomic (temp file + rename).

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};


pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

pub fn app_dir() -> PathBuf {
    local_app_data().join("Valolysis")
}

pub fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where the VALORANT client saves downloaded replays.
pub fn default_demos_dir() -> PathBuf {
    local_app_data()
        .join("VALORANT")
        .join("Saved")
        .join("Demos")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub api: String,
    /// Upload new replays as soon as they finish downloading.
    pub auto_upload: bool,
    /// Count uploads toward the public dataset.
    pub publish: bool,
    pub redact_names: bool,
    pub redact_pids: bool,
    pub notifications: bool,
    /// Whether to move replays to the Recycle Bin once they are uploaded.
    pub delete_after: DeleteAfter,
    /// Overrides the VALORANT Demos folder.
    pub demos_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api: "https://valolysis.odinnichols.dev".into(),
            auto_upload: true,
            publish: true,
            redact_names: false,
            redact_pids: false,
            notifications: true,
            delete_after: DeleteAfter::Never,
            demos_dir: None,
        }
    }
}

impl Settings {
    pub fn demos_dir(&self) -> PathBuf {
        self.demos_dir.clone().unwrap_or_else(default_demos_dir)
    }

    pub fn api(&self) -> &str {
        self.api.trim_end_matches('/')
    }
}

/// When a local replay may be moved to the Recycle Bin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteAfter {
    #[default]
    Never,
    /// As soon as the server has the complete file.
    Uploaded,
    /// Only after the server parsed it successfully, so failed replays stay.
    Processed,
}

/// What happened to the replay file on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalFile {
    #[default]
    Present,
    /// Moved to the Recycle Bin by this app.
    Recycled,
    /// Removed by something else (the game or the user).
    Missing,
    /// Changed on disk since it was uploaded, so it is never deleted.
    Kept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Present before the first run; uploaded only on request.
    Existing,
    /// Waiting to upload (possibly after a retry delay).
    Pending,
    /// Uploaded; waiting for the server to finish processing.
    Processing,
    Ready,
    /// Same bytes as a replay that was already uploaded.
    Duplicate,
    /// Permanent failure (rejected by the server or by the parser).
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub modified: u64,
    pub status: Status,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// Unix seconds before which the entry is not retried or polled.
    #[serde(default)]
    pub not_before: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub local: LocalFile,
}

impl Entry {
    pub fn new(size: u64, modified: u64, status: Status) -> Self {
        Self {
            size,
            modified,
            status,
            sha256: None,
            job_id: None,
            error: None,
            attempts: 0,
            not_before: 0,
            updated_at: now_secs(),
            local: LocalFile::Present,
        }
    }
}

/// What has happened to every replay file the app has seen, keyed by
/// lowercase file name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    /// Set once the files present at first launch have been recorded.
    pub baseline_recorded: bool,
    pub files: BTreeMap<String, Entry>,
}

impl Ledger {
    pub fn key(name: &str) -> String {
        name.to_lowercase()
    }

    /// The file that was already uploaded with these exact bytes, if any.
    pub fn uploaded_with_hash(&self, sha256: &str, except: &str) -> Option<&str> {
        self.files.iter().find_map(|(name, entry)| {
            (name != except
                && entry.sha256.as_deref() == Some(sha256)
                && matches!(entry.status, Status::Processing | Status::Ready))
            .then_some(name.as_str())
        })
    }

    /// True when `mode` allows deleting the local copy behind `entry`.
    /// A duplicate qualifies through the upload that carries its bytes.
    pub fn deletable(&self, entry: &Entry, mode: DeleteAfter) -> bool {
        let qualifies = |status: Status| match mode {
            DeleteAfter::Never => false,
            DeleteAfter::Uploaded => matches!(status, Status::Processing | Status::Ready),
            DeleteAfter::Processed => status == Status::Ready,
        };
        if entry.local != LocalFile::Present || entry.sha256.is_none() {
            return false;
        }
        match entry.status {
            Status::Duplicate => self.files.values().any(|other| {
                other.sha256 == entry.sha256
                    && other.status != Status::Duplicate
                    && qualifies(other.status)
            }),
            status => qualifies(status),
        }
    }

    pub fn count(&self, status: Status) -> usize {
        self.files
            .values()
            .filter(|entry| entry.status == status)
            .count()
    }
}

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("ignoring unreadable {}: {error}", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_tolerate_missing_and_unknown_fields() {
        let settings: Settings =
            serde_json::from_str(r#"{"publish": false, "future": 1}"#).unwrap();
        assert!(!settings.publish);
        assert!(settings.auto_upload);
        assert_eq!(settings.api, "https://valolysis.odinnichols.dev".into());
    }

    #[test]
    fn duplicate_detection_ignores_unfinished_entries() {
        let mut ledger = Ledger::default();
        let mut uploaded = Entry::new(1, 1, Status::Ready);
        uploaded.sha256 = Some("abc".into());
        ledger.files.insert("a.vrf".into(), uploaded);
        let mut pending = Entry::new(1, 1, Status::Pending);
        pending.sha256 = Some("def".into());
        ledger.files.insert("b.vrf".into(), pending);
        assert_eq!(ledger.uploaded_with_hash("abc", "c.vrf"), Some("a.vrf"));
        assert_eq!(ledger.uploaded_with_hash("abc", "a.vrf"), None);
        assert_eq!(ledger.uploaded_with_hash("def", "c.vrf"), None);
    }

    #[test]
    fn deletion_follows_the_chosen_mode() {
        let mut ledger = Ledger::default();
        let entry = |status, sha: &str| {
            let mut entry = Entry::new(1, 1, status);
            entry.sha256 = Some(sha.into());
            entry
        };
        ledger
            .files
            .insert("uploaded.vrf".into(), entry(Status::Processing, "a"));
        ledger
            .files
            .insert("processed.vrf".into(), entry(Status::Ready, "b"));
        ledger
            .files
            .insert("failed.vrf".into(), entry(Status::Failed, "c"));
        ledger
            .files
            .insert("earlier.vrf".into(), entry(Status::Existing, "d"));
        ledger
            .files
            .insert("copy.vrf".into(), entry(Status::Duplicate, "a"));
        let deletable = |mode| {
            ledger
                .files
                .iter()
                .filter(|(_, e)| ledger.deletable(e, mode))
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
        };
        assert!(deletable(DeleteAfter::Never).is_empty());
        assert_eq!(
            deletable(DeleteAfter::Uploaded),
            ["copy.vrf", "processed.vrf", "uploaded.vrf"]
        );
        assert_eq!(deletable(DeleteAfter::Processed), ["processed.vrf"]);

        let mut recycled = entry(Status::Ready, "e");
        recycled.local = LocalFile::Recycled;
        assert!(!ledger.deletable(&recycled, DeleteAfter::Uploaded));
        let mut kept = entry(Status::Ready, "f");
        kept.local = LocalFile::Kept;
        assert!(!ledger.deletable(&kept, DeleteAfter::Uploaded));
    }

    #[test]
    fn older_ledgers_load_with_files_present() {
        let entry: Entry =
            serde_json::from_str(r#"{"size":1,"modified":2,"status":"ready"}"#).unwrap();
        assert_eq!(entry.local, LocalFile::Present);
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.delete_after, DeleteAfter::Never);
    }

    #[test]
    fn save_and_load_round_trip() {
        let path =
            std::env::temp_dir().join(format!("valolysis-store-{}.json", std::process::id()));
        let mut ledger = Ledger::default();
        ledger
            .files
            .insert("x.vrf".into(), Entry::new(5, 6, Status::Existing));
        save(&path, &ledger).unwrap();
        assert_eq!(load::<Ledger>(&path), ledger);
        let _ = fs::remove_file(path);
    }
}
