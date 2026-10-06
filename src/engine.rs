//! Background engine: watches the replay folder, uploads finished replays and
//! follows their processing. Runs on its own thread; the tray talks to it
//! through `Command`s and reads a shared `Snapshot`.

use crate::{
    api::{Api, ApiError, UploadRequest},
    login,
    platform::{self, Credentials},
    store::{self, DeleteAfter, Entry, Ledger, LocalFile, Settings, Status, now_secs},
};
use notify::{RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

/// A replay must keep the same size and timestamp this long, and be openable
/// without another writer, before it is treated as fully downloaded.
const STABLE_FOR: Duration = Duration::from_secs(5);
const RESCAN_EVERY: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_secs(2);
const FILE_SHARE_READ: u32 = 1;

pub enum Command {
    SignIn,
    CancelSignIn,
    SignedIn(Result<crate::api::DesktopSession, String>),
    SignOut,
    SetAutoUpload(bool),
    SetPublish(bool),
    SetRedactNames(bool),
    SetRedactPids(bool),
    SetNotifications(bool),
    SetDeleteAfter(DeleteAfter),
    UploadExisting,
    RetryFailed,
    FolderChanged,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub title: String,
    pub body: String,
    pub warning: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub account: Option<String>,
    pub signed_in: bool,
    pub signing_in: bool,
    pub settings: Settings,
    pub activity: String,
    pub folder_found: bool,
    pub pending: usize,
    pub processing: usize,
    pub ready: usize,
    pub existing: usize,
    pub failed: usize,
    pub notices: Vec<Notice>,
}

pub struct Shared {
    pub snapshot: Mutex<Snapshot>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            snapshot: Mutex::new(Snapshot::default()),
            wake: Box::new(wake),
        }
    }

    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut self.snapshot.lock().unwrap().notices)
    }
}

pub struct Candidate {
    size: u64,
    modified: u64,
    stable_since: Instant,
}

pub struct Engine {
    settings: Settings,
    ledger: Ledger,
    settings_path: PathBuf,
    ledger_path: PathBuf,
    api: Api,
    credentials: Credentials,
    token: Option<String>,
    account: Option<String>,
    shared: Arc<Shared>,
    commands: Sender<Command>,
    candidates: HashMap<String, Candidate>,
    watcher: Option<(PathBuf, notify::RecommendedWatcher)>,
    last_scan: Option<Instant>,
    scan_requested: bool,
    sign_in_cancel: Option<Arc<AtomicBool>>,
    activity: String,
    notices: Vec<Notice>,
}

impl Engine {
    pub fn new(shared: Arc<Shared>, commands: Sender<Command>) -> Self {
        let dir = store::app_dir();
        let settings_path = dir.join("settings.json");
        let ledger_path = dir.join("uploads.json");
        let settings: Settings = store::load(&settings_path);
        let ledger: Ledger = store::load(&ledger_path);
        let api = Api::new(settings.api());
        let credentials = Credentials::for_api(settings.api());
        let token = credentials.read();
        Self {
            settings,
            ledger,
            settings_path,
            ledger_path,
            api,
            credentials,
            token,
            account: None,
            shared,
            commands,
            candidates: HashMap::new(),
            watcher: None,
            last_scan: None,
            scan_requested: true,
            sign_in_cancel: None,
            activity: "Starting".into(),
            notices: Vec::new(),
        }
    }

    pub fn run(mut self, commands: Receiver<Command>) {
        if !self.settings_path.exists() {
            self.save_settings();
        }
        self.refresh_account();
        self.publish();
        loop {
            match commands.recv_timeout(TICK) {
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(command) = commands.try_recv() {
                self.handle(command);
            }
            self.ensure_watcher();
            self.scan_if_due();
            self.poll_processing();
            self.remove_uploaded_replays();
            self.upload_next();
            self.publish();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::SignIn => self.begin_sign_in(),
            Command::CancelSignIn => {
                if let Some(cancel) = self.sign_in_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            Command::SignedIn(result) => {
                self.sign_in_cancel = None;
                match result {
                    Ok(session) => {
                        if let Err(error) = self.credentials.write(&session.token) {
                            log::error!("could not store session: {error}");
                            self.notify(
                                "Sign-in failed",
                                "Could not save the session in Credential Manager.",
                                true,
                            );
                            return;
                        }
                        self.token = Some(session.token);
                        self.account = session.user.email.clone();
                        let who = session.user.email.unwrap_or_else(|| "your account".into());
                        self.notify("Signed in", &format!("Uploading replays as {who}."), false);
                        self.scan_requested = true;
                    }
                    Err(error) => self.notify("Sign-in failed", &error, true),
                }
            }
            Command::SignOut => {
                if let Some(token) = self.token.take()
                    && let Err(error) = self.api.logout(&token)
                {
                    log::warn!("logout request failed: {error}");
                }
                self.credentials.delete();
                self.account = None;
            }
            Command::SetAutoUpload(value) => self.update_settings(|s| s.auto_upload = value),
            Command::SetPublish(value) => self.update_settings(|s| s.publish = value),
            Command::SetRedactNames(value) => self.update_settings(|s| s.redact_names = value),
            Command::SetRedactPids(value) => self.update_settings(|s| s.redact_pids = value),
            Command::SetNotifications(value) => self.update_settings(|s| s.notifications = value),
            Command::SetDeleteAfter(mode) => self.update_settings(|s| s.delete_after = mode),
            Command::UploadExisting => {
                for entry in self
                    .ledger
                    .files
                    .values_mut()
                    .filter(|e| e.status == Status::Existing)
                {
                    entry.status = Status::Pending;
                    entry.not_before = 0;
                }
                self.save_ledger();
            }
            Command::RetryFailed => {
                for entry in self
                    .ledger
                    .files
                    .values_mut()
                    .filter(|e| e.status == Status::Failed)
                {
                    entry.status = Status::Pending;
                    entry.attempts = 0;
                    entry.not_before = 0;
                    entry.error = None;
                }
                self.save_ledger();
            }
            Command::FolderChanged => self.scan_requested = true,
        }
    }

    fn begin_sign_in(&mut self) {
        if self.sign_in_cancel.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.sign_in_cancel = Some(cancel.clone());
        let api = Api::new(self.settings.api());
        let sender = self.commands.clone();
        std::thread::spawn(move || {
            let result = login::sign_in(&api, platform::open, &cancel);
            let _ = sender.send(Command::SignedIn(result));
        });
    }

    fn refresh_account(&mut self) {
        let Some(token) = self.token.clone() else {
            return;
        };
        match self.api.profile(&token) {
            Ok(profile) => self.account = profile.user.email,
            Err(ApiError::Unauthorized) => self.session_expired(),
            Err(error) => log::warn!("could not load account: {error}"),
        }
    }

    fn session_expired(&mut self) {
        self.token = None;
        self.account = None;
        self.credentials.delete();
        self.notify(
            "Sign in again",
            "Your Valoingest session expired. Uploads are paused until you sign in.",
            true,
        );
    }

    fn update_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        change(&mut self.settings);
        self.save_settings();
    }

    fn save_settings(&self) {
        if let Err(error) = store::save(&self.settings_path, &self.settings) {
            log::error!("could not save settings: {error}");
        }
    }

    fn save_ledger(&self) {
        if let Err(error) = store::save(&self.ledger_path, &self.ledger) {
            log::error!("could not save upload history: {error}");
        }
    }

    fn notify(&mut self, title: &str, body: &str, warning: bool) {
        log::info!("{title}: {body}");
        if self.settings.notifications || warning {
            self.notices.push(Notice {
                title: title.into(),
                body: body.into(),
                warning,
            });
        }
    }

    fn publish(&mut self) {
        let folder_found = self.settings.demos_dir().is_dir();
        {
            let mut snapshot = self.shared.snapshot.lock().unwrap();
            snapshot.account = self.account.clone();
            snapshot.signed_in = self.token.is_some();
            snapshot.signing_in = self.sign_in_cancel.is_some();
            snapshot.settings = self.settings.clone();
            snapshot.activity = self.activity.clone();
            snapshot.folder_found = folder_found;
            snapshot.pending = self.ledger.count(Status::Pending);
            snapshot.processing = self.ledger.count(Status::Processing);
            snapshot.ready = self.ledger.count(Status::Ready);
            snapshot.existing = self.ledger.count(Status::Existing);
            snapshot.failed = self.ledger.count(Status::Failed);
            snapshot.notices.append(&mut self.notices);
        }
        (self.shared.wake)();
    }

    fn set_activity(&mut self, activity: impl Into<String>) {
        let activity = activity.into();
        if activity != self.activity {
            self.activity = activity;
            self.publish();
        }
    }

    // ---------- Watching ----------

    fn ensure_watcher(&mut self) {
        let dir = self.settings.demos_dir();
        if self
            .watcher
            .as_ref()
            .is_some_and(|(watched, _)| *watched == dir)
            || !dir.is_dir()
        {
            return;
        }
        let sender = self.commands.clone();
        let created = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                let _ = sender.send(Command::FolderChanged);
            }
        })
        .and_then(|mut watcher| {
            watcher
                .watch(&dir, RecursiveMode::NonRecursive)
                .map(|_| watcher)
        });
        match created {
            Ok(watcher) => {
                log::info!("watching {}", dir.display());
                self.watcher = Some((dir, watcher));
                self.scan_requested = true;
            }
            Err(error) => log::warn!("could not watch {}: {error}", dir.display()),
        }
    }

    fn scan_if_due(&mut self) {
        let due = self.scan_requested
            || !self.candidates.is_empty()
            || self
                .last_scan
                .is_none_or(|last| last.elapsed() >= RESCAN_EVERY);
        if !due {
            return;
        }
        self.scan_requested = false;
        self.last_scan = Some(Instant::now());
        let dir = self.settings.demos_dir();
        let newly_ready = scan(&dir, &mut self.ledger, &mut self.candidates, STABLE_FOR);
        if newly_ready.changed {
            self.save_ledger();
        }
        if newly_ready.added > 0 && !self.settings.auto_upload {
            let count = newly_ready.added;
            self.notify(
                "New replays found",
                &format!("{count} new replay(s). Automatic upload is paused."),
                false,
            );
        }
    }

    // ---------- Uploading ----------

    fn upload_next(&mut self) {
        let Some(token) = self.token.clone() else {
            if self.ledger.count(Status::Pending) > 0 {
                self.set_activity("Sign in to upload");
            } else {
                self.set_activity("Signed out");
            }
            return;
        };
        if !self.settings.auto_upload {
            self.set_activity("Paused");
            return;
        }
        let now = now_secs();
        let next = self
            .ledger
            .files
            .iter()
            .filter(|(_, entry)| entry.status == Status::Pending && entry.not_before <= now)
            .min_by_key(|(_, entry)| entry.modified)
            .map(|(name, _)| name.clone());
        let Some(name) = next else {
            let waiting = self.ledger.count(Status::Pending);
            if waiting > 0 {
                let soonest = self
                    .ledger
                    .files
                    .values()
                    .filter(|e| e.status == Status::Pending)
                    .map(|e| e.not_before)
                    .min()
                    .unwrap_or(now);
                let reason = self.ledger.files.values().find_map(|e| {
                    (e.status == Status::Pending)
                        .then(|| e.error.clone())
                        .flatten()
                });
                let minutes = soonest.saturating_sub(now).div_ceil(60);
                self.set_activity(match reason {
                    Some(reason) => {
                        format!("{waiting} waiting ({reason}); retrying in {minutes} min")
                    }
                    None => format!("{waiting} waiting; retrying in {minutes} min"),
                });
            } else {
                self.set_activity("Watching for new replays");
            }
            return;
        };
        self.upload(&name, &token);
        self.save_ledger();
    }

    fn upload(&mut self, key: &str, token: &str) {
        let path = self.settings.demos_dir().join(key);
        let path = find_case_insensitive(&path).unwrap_or(path);
        let display = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.to_owned());
        let Ok(metadata) = fs::metadata(&path) else {
            self.ledger.files.remove(key);
            return;
        };

        // Cheap precheck: never spend quota or bandwidth on something that is
        // not a VALORANT replay.
        if !is_replay_file(&path) {
            if let Some(entry) = self.ledger.files.get_mut(key) {
                entry.status = Status::Failed;
                entry.error = Some("not a VALORANT replay file".into());
            }
            self.notify(
                "Skipped file",
                &format!("{display} is not a VALORANT replay."),
                true,
            );
            return;
        }

        self.set_activity(format!("Hashing {display}"));
        let sha256 = match hash_file(&path) {
            Ok(hash) => hash,
            Err(error) => {
                self.retry_later(key, &format!("could not read file: {error}"));
                return;
            }
        };
        if let Some(original) = self
            .ledger
            .uploaded_with_hash(&sha256, key)
            .map(str::to_owned)
        {
            let entry = self.ledger.files.get_mut(key).unwrap();
            entry.sha256 = Some(sha256);
            entry.status = Status::Duplicate;
            entry.error = Some(format!("same file as {original}"));
            return;
        }
        if let Some(entry) = self.ledger.files.get_mut(key) {
            entry.sha256 = Some(sha256.clone());
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| is_component(s));
        let request = UploadRequest {
            size: metadata.len(),
            sha256: &sha256,
            source_replay_id: stem,
            redact_names: self.settings.redact_names,
            redact_pids: self.settings.redact_pids,
            publish: self.settings.publish,
        };
        let created = match self.api.create_upload(token, &request) {
            Ok(created) => created,
            Err(error) => return self.upload_failed(key, &display, error),
        };

        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                let _ = self.api.abort_upload(token, &created.upload_id);
                return self.retry_later(key, &format!("could not read file: {error}"));
            }
        };
        let mut buffer = vec![0_u8; created.part_size as usize];
        let mut etags: Vec<(u32, String)> = Vec::new();
        for part in 1..=created.part_count {
            let expected = if part < created.part_count {
                created.part_size
            } else {
                metadata.len() - created.part_size * u64::from(created.part_count - 1)
            } as usize;
            if let Err(error) = file.read_exact(&mut buffer[..expected]) {
                let _ = self.api.abort_upload(token, &created.upload_id);
                return self.retry_later(key, &format!("file changed while uploading: {error}"));
            }
            let percent = part * 100 / created.part_count;
            self.set_activity(format!("Uploading {display} ({percent}%)"));
            // Direct mode sends the bytes straight to R2; otherwise the Worker proxies them.
            let direct = created
                .parts
                .as_ref()
                .and_then(|parts| parts.iter().find(|p| p.part_number == part));
            let result = match direct {
                Some(direct) if direct.size as usize == expected => self
                    .api
                    .upload_direct_part(&direct.url, &buffer[..expected])
                    .map(|etag| etags.push((part, etag))),
                Some(_) => Err(crate::api::ApiError::Transient(
                    "server part sizes disagree with the file".into(),
                )),
                None => self
                    .api
                    .upload_part(token, &created.upload_id, part, &buffer[..expected]),
            };
            if let Err(error) = result {
                let _ = self.api.abort_upload(token, &created.upload_id);
                return self.upload_failed(key, &display, error);
            }
        }
        if let Err(error) = self.api.complete_upload(token, &created.upload_id, &etags) {
            return self.upload_failed(key, &display, error);
        }

        let entry = self.ledger.files.get_mut(key).unwrap();
        entry.status = Status::Processing;
        entry.job_id = Some(created.job_id);
        entry.error = None;
        entry.attempts = 0;
        entry.not_before = now_secs() + 20;
        entry.updated_at = now_secs();
        let destination = if self.settings.publish {
            "published"
        } else {
            "private"
        };
        self.notify(
            "Replay uploaded",
            &format!("{display} uploaded ({destination}). Processing now."),
            false,
        );
    }

    fn upload_failed(&mut self, key: &str, display: &str, error: ApiError) {
        log::warn!("upload of {display} failed: {error}");
        match error {
            ApiError::Unauthorized => self.session_expired(),
            ApiError::RateLimited(message) => {
                let wait = if message.contains("daily") {
                    seconds_until_utc_midnight(now_secs()) + 60
                } else {
                    120
                };
                if let Some(entry) = self.ledger.files.get_mut(key) {
                    entry.not_before = now_secs() + wait;
                    entry.error = Some(message);
                }
            }
            ApiError::Rejected(_, message) => {
                if let Some(entry) = self.ledger.files.get_mut(key) {
                    entry.status = Status::Failed;
                    entry.error = Some(message.clone());
                }
                self.notify("Upload rejected", &format!("{display}: {message}"), true);
            }
            ApiError::Transient(message) => self.retry_later(key, &message),
        }
    }

    fn retry_later(&mut self, key: &str, message: &str) {
        if let Some(entry) = self.ledger.files.get_mut(key) {
            entry.attempts += 1;
            entry.not_before = now_secs() + backoff_seconds(entry.attempts);
            entry.error = Some(message.to_owned());
        }
    }

    // ---------- Processing ----------

    fn poll_processing(&mut self) {
        let Some(token) = self.token.clone() else {
            return;
        };
        let now = now_secs();
        let due: Vec<(String, String)> = self
            .ledger
            .files
            .iter()
            .filter(|(_, e)| e.status == Status::Processing && e.not_before <= now)
            .filter_map(|(name, e)| Some((name.clone(), e.job_id.clone()?)))
            .collect();
        let mut changed = false;
        for (key, job_id) in due {
            let job = match self.api.job(&token, &job_id) {
                Ok(job) => job,
                Err(ApiError::Unauthorized) => return self.session_expired(),
                Err(error) => {
                    log::warn!("could not check {key}: {error}");
                    if let Some(entry) = self.ledger.files.get_mut(&key) {
                        entry.not_before = now + 120;
                    }
                    continue;
                }
            };
            changed = true;
            let entry = self.ledger.files.get_mut(&key).unwrap();
            match job.status.as_str() {
                "ready" => {
                    entry.status = Status::Ready;
                    entry.updated_at = now;
                    let rounds = job
                        .rounds
                        .map(|r| format!("{r} rounds"))
                        .unwrap_or_else(|| "rounds unknown".into());
                    let kills = job
                        .kills
                        .map(|k| format!(", {k} kills"))
                        .unwrap_or_default();
                    let dataset = match job.competitive {
                        Some(true) => "",
                        Some(false) => " Not a competitive game, so it stays out of the dataset.",
                        None => " Queue unknown, so it stays out of the dataset.",
                    };
                    self.notify(
                        "Replay processed",
                        &format!("{key}: {rounds}{kills}.{dataset}"),
                        false,
                    );
                }
                "failed" | "aborted" => {
                    entry.status = Status::Failed;
                    entry.error = job.error_code.clone();
                    let code = job.error_code.unwrap_or_else(|| job.status.clone());
                    self.notify("Processing failed", &format!("{key}: {code}"), true);
                }
                _ => {
                    let age = now.saturating_sub(entry.updated_at);
                    entry.not_before = now + if age > 3_600 { 300 } else { 30 };
                }
            }
        }
        if changed {
            self.save_ledger();
        }
    }
}

impl Engine {
    /// Moves uploaded replays to the Recycle Bin when the chosen mode allows
    /// it. A file is removed only if it is still exactly what was uploaded.
    fn remove_uploaded_replays(&mut self) {
        let mode = self.settings.delete_after;
        if mode == DeleteAfter::Never {
            return;
        }
        let due: Vec<String> = self
            .ledger
            .files
            .iter()
            .filter(|(_, entry)| self.ledger.deletable(entry, mode))
            .map(|(name, _)| name.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        let dir = self.settings.demos_dir();
        let mut recycled = 0;
        for key in due {
            let entry = self.ledger.files.get(&key).cloned().unwrap();
            let outcome = match find_case_insensitive(&dir.join(&key)) {
                Some(path) if path.exists() => {
                    if unchanged_since_upload(&path, &entry) {
                        match platform::recycle(&path) {
                            Ok(()) => {
                                recycled += 1;
                                LocalFile::Recycled
                            }
                            Err(error) => {
                                log::warn!("could not remove {key}: {error}");
                                // Try again on a later pass.
                                continue;
                            }
                        }
                    } else {
                        log::warn!("{key} changed since upload; keeping it");
                        LocalFile::Kept
                    }
                }
                _ => LocalFile::Missing,
            };
            if let Some(entry) = self.ledger.files.get_mut(&key) {
                entry.local = outcome;
            }
        }
        self.save_ledger();
        if recycled > 0 {
            let noun = if recycled == 1 { "replay" } else { "replays" };
            self.notify(
                "Replays removed",
                &format!("Moved {recycled} uploaded {noun} to the Recycle Bin."),
                false,
            );
        }
    }
}

/// Same size, timestamp and SHA-256 as the copy that was uploaded.
fn unchanged_since_upload(path: &Path, entry: &Entry) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    metadata.len() == entry.size
        && modified == entry.modified
        && entry
            .sha256
            .as_deref()
            .is_some_and(|expected| hash_file(path).is_ok_and(|actual| actual == expected))
}

// ---------- Pure helpers (unit tested) ----------

pub struct ScanResult {
    pub added: usize,
    pub changed: bool,
}

/// Reconciles the folder with the ledger. On the first ever scan, existing
/// replays are recorded as `Existing` (not uploaded). Afterwards, new files
/// become `Pending` once they have been stable for `stable_for` and no other
/// process is writing them.
pub fn scan(
    dir: &Path,
    ledger: &mut Ledger,
    candidates: &mut HashMap<String, Candidate>,
    stable_for: Duration,
) -> ScanResult {
    let mut result = ScanResult {
        added: 0,
        changed: false,
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return result;
    };
    let baseline = !ledger.baseline_recorded;
    let mut present = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_replay = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("vrf"));
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !is_replay || !metadata.is_file() {
            continue;
        }
        let key = Ledger::key(&entry.file_name().to_string_lossy());
        present.push(key.clone());
        let size = metadata.len();
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        if ledger.files.contains_key(&key) {
            continue;
        }
        if baseline {
            ledger
                .files
                .insert(key, Entry::new(size, modified, Status::Existing));
            result.changed = true;
            continue;
        }
        let stable = match candidates.get_mut(&key) {
            Some(candidate) if candidate.size == size && candidate.modified == modified => {
                candidate.stable_since.elapsed() >= stable_for && not_being_written(&path)
            }
            Some(candidate) => {
                *candidate = Candidate {
                    size,
                    modified,
                    stable_since: Instant::now(),
                };
                false
            }
            None => {
                candidates.insert(
                    key.clone(),
                    Candidate {
                        size,
                        modified,
                        stable_since: Instant::now(),
                    },
                );
                stable_for.is_zero() && not_being_written(&path)
            }
        };
        if stable && size > 0 {
            candidates.remove(&key);
            ledger
                .files
                .insert(key, Entry::new(size, modified, Status::Pending));
            result.added += 1;
            result.changed = true;
        }
    }
    candidates.retain(|key, _| present.contains(key));
    if baseline {
        ledger.baseline_recorded = true;
        result.changed = true;
    }
    result
}

/// True when the file can be opened while denying other writers, i.e. the
/// game has finished writing it.
fn not_being_written(path: &Path) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .is_ok()
}

fn find_case_insensitive(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return Some(path.to_path_buf());
    }
    let wanted = path.file_name()?.to_string_lossy().to_lowercase();
    fs::read_dir(path.parent()?)
        .ok()?
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().to_lowercase() == wanted)
        .map(|entry| entry.path())
}

/// Unreal local-file replay magic, the first four bytes of every .vrf file.
const REPLAY_MAGIC: u32 = 0x43F4_EFDD;

/// True when the file starts with the replay magic. Version fields are not
/// checked: they change with game patches and the server parser decides.
pub fn is_replay_file(path: &Path) -> bool {
    let mut magic = [0_u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok_and(|()| u32::from_le_bytes(magic) == REPLAY_MAGIC)
}

pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn backoff_seconds(attempts: u32) -> u64 {
    (30_u64 << attempts.min(7)).min(3_600)
}

pub fn seconds_until_utc_midnight(now: u64) -> u64 {
    86_400 - now % 86_400
}

fn is_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("valoingest-engine-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_scan_records_existing_replays_without_uploading() {
        let dir = temp_dir("baseline");
        fs::write(dir.join("Old.VRF"), b"old").unwrap();
        fs::write(dir.join("notes.txt"), b"ignore").unwrap();
        let mut ledger = Ledger::default();
        let mut candidates = HashMap::new();
        let result = scan(&dir, &mut ledger, &mut candidates, Duration::ZERO);
        assert_eq!(result.added, 0);
        assert!(ledger.baseline_recorded);
        assert_eq!(
            ledger.files.get("old.vrf").map(|e| e.status),
            Some(Status::Existing)
        );
        assert_eq!(ledger.files.len(), 1);

        fs::write(dir.join("new.vrf"), b"new replay").unwrap();
        let result = scan(&dir, &mut ledger, &mut candidates, Duration::ZERO);
        assert_eq!(result.added, 1);
        assert_eq!(
            ledger.files.get("new.vrf").map(|e| e.status),
            Some(Status::Pending)
        );
        // Already known files are never re-queued.
        assert_eq!(
            scan(&dir, &mut ledger, &mut candidates, Duration::ZERO).added,
            0
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn growing_or_locked_files_wait() {
        let dir = temp_dir("stable");
        let mut ledger = Ledger {
            baseline_recorded: true,
            ..Default::default()
        };
        let mut candidates = HashMap::new();
        let path = dir.join("downloading.vrf");
        fs::write(&path, b"part").unwrap();
        assert_eq!(
            scan(&dir, &mut ledger, &mut candidates, Duration::from_secs(60)).added,
            0
        );
        assert!(candidates.contains_key("downloading.vrf"));

        // Still open for writing by "the game": not ready even when stable.
        let writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        candidates.get_mut("downloading.vrf").unwrap().stable_since =
            Instant::now() - Duration::from_secs(120);
        assert_eq!(
            scan(&dir, &mut ledger, &mut candidates, Duration::from_secs(60)).added,
            0
        );
        drop(writer);
        assert_eq!(
            scan(&dir, &mut ledger, &mut candidates, Duration::from_secs(60)).added,
            1
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_files_are_not_uploaded() {
        let dir = temp_dir("empty");
        let mut ledger = Ledger {
            baseline_recorded: true,
            ..Default::default()
        };
        fs::write(dir.join("empty.vrf"), b"").unwrap();
        assert_eq!(
            scan(&dir, &mut ledger, &mut HashMap::new(), Duration::ZERO).added,
            0
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hashes_match_sha256() {
        let dir = temp_dir("hash");
        let path = dir.join("a.vrf");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            hash_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_uploaded_bytes_are_deleted() {
        let dir = temp_dir("delete");
        let path = dir.join("match.vrf");
        fs::write(&path, b"abc").unwrap();
        let mut ledger = Ledger {
            baseline_recorded: true,
            ..Default::default()
        };
        scan(&dir, &mut ledger, &mut HashMap::new(), Duration::ZERO);
        let mut entry = ledger.files["match.vrf"].clone();
        entry.sha256 = Some(hash_file(&path).unwrap());
        assert!(unchanged_since_upload(&path, &entry));

        entry.sha256 = Some("0".repeat(64));
        assert!(!unchanged_since_upload(&path, &entry), "different bytes");
        entry.sha256 = Some(hash_file(&path).unwrap());
        entry.size += 1;
        assert!(!unchanged_since_upload(&path, &entry), "different size");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replay_precheck_reads_only_the_magic() {
        let dir = temp_dir("magic");
        let replay = dir.join("real.vrf");
        let mut bytes = REPLAY_MAGIC.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[7, 0, 0, 0]);
        fs::write(&replay, &bytes).unwrap();
        assert!(is_replay_file(&replay));
        let fake = dir.join("renamed.vrf");
        fs::write(&fake, b"PK\x03\x04 a zip").unwrap();
        assert!(!is_replay_file(&fake));
        let short = dir.join("short.vrf");
        fs::write(&short, [0xDD, 0xEF]).unwrap();
        assert!(!is_replay_file(&short));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn retry_timing() {
        assert_eq!(backoff_seconds(1), 60);
        assert_eq!(backoff_seconds(20), 3_600);
        assert_eq!(seconds_until_utc_midnight(86_400 * 3 + 86_399), 1);
        assert!(is_component("e383f7c8-f80b-43ac-a1f0-d850d044c300"));
        assert!(!is_component("bad name"));
    }
}
