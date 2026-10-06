//! Startup updates from this app's public GitHub releases. Downloading runs
//! separately from uploads; a verified copy of the new app acts as the helper.

use crate::{engine::Command, platform::wide};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand},
    ptr,
    sync::mpsc::Sender,
    time::{Duration, Instant},
};
use ureq::Agent;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    Storage::FileSystem::ReplaceFileW,
    System::Threading::{CREATE_NO_WINDOW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
};

const REPOSITORY: &str = "B1Fr0st/valolysis-desktop";
const ASSET_NAME: &str = "valolysis-windows-x64.exe";
const STAGE_PREFIX: &str = ".valolysis-update-";
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const STAGE_FILES: &[&str] = &[
    "helper.exe",
    "replacement.exe",
    "previous.exe",
    "update.json",
    "ready",
    "complete",
];
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateJob {
    target: PathBuf,
    parent_pid: u32,
    version: String,
    sha256: String,
    original_sha256: String,
}

pub struct PreparedUpdate {
    stage: PathBuf,
    handed_off: bool,
}

impl Drop for PreparedUpdate {
    fn drop(&mut self) {
        if !self.handed_off {
            remove_stage_files(&self.stage);
        }
    }
}

impl PreparedUpdate {
    /// The readiness handshake proves the helper has opened the parent process
    /// handle before we let that process exit (avoids PID-reuse and exit races).
    pub fn launch(mut self) -> Result<()> {
        let mut helper = spawn(&self.stage.join("helper.exe"), &["--apply-update"])?;
        let start = Instant::now();
        while !self.stage.join("ready").exists() {
            if let Some(status) = helper.try_wait()? {
                return Err(format!("update helper exited before readiness: {status}").into());
            }
            if start.elapsed() >= HANDSHAKE_TIMEOUT {
                let _ = helper.kill();
                let _ = helper.wait();
                return Err("update helper did not become ready".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.handed_off = true;
        log::info!("update helper ready; exiting to install the new version");
        Ok(())
    }
}

pub fn start(commands: Sender<Command>) {
    let result = std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || match prepare(env!("CARGO_PKG_VERSION")) {
            Ok(Some(update)) => {
                let _ = commands.send(Command::UpdateReady(Box::new(update)));
            }
            Ok(None) => log::info!("no newer desktop release available"),
            Err(error) => log::warn!("startup update check skipped: {error}"),
        });
    if let Err(error) = result {
        log::warn!("could not start update check: {error}");
    }
}

fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(5)))
        .user_agent(concat!("valolysis-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn latest_release() -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let mut response = match agent(Duration::from_secs(10))
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()?;
    Ok(Some(serde_json::from_str(&body)?))
}

fn select_asset<'a>(release: &'a Release, current: &str) -> Result<Option<(&'a Asset, String)>> {
    let current = Version::parse(current)?;
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .ok_or("invalid release tag")?,
    )?;
    if release.draft || release.prerelease || !version.pre.is_empty() || version <= current {
        return Ok(None);
    }
    if !version.build.is_empty() {
        return Err("release has unexpected build metadata".into());
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == ASSET_NAME)
        .ok_or("release has no Windows x64 executable")?;
    let expected_url =
        format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{ASSET_NAME}");
    if asset.browser_download_url != expected_url
        || asset.size == 0
        || asset.size > MAX_DOWNLOAD_BYTES
    {
        return Err("invalid release asset URL or size".into());
    }
    digest(asset)?;
    Ok(Some((asset, version.to_string())))
}

fn digest(asset: &Asset) -> Result<&str> {
    let hash = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or("release asset is missing GitHub's SHA-256 digest")?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid release SHA-256 digest".into());
    }
    Ok(hash)
}

fn prepare(current: &str) -> Result<Option<PreparedUpdate>> {
    let Some(release) = latest_release()? else {
        return Ok(None);
    };
    let Some((asset, version)) = select_asset(&release, current)? else {
        return Ok(None);
    };
    let target = fs::canonicalize(std::env::current_exe()?)?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let stage = target
        .parent()
        .ok_or("no executable directory")?
        .join(format!("{STAGE_PREFIX}{suffix}"));
    // Staging on the same volume also proves the install directory is writable.
    fs::create_dir(&stage)?;
    let update = PreparedUpdate {
        stage,
        handed_off: false,
    };
    log::info!("downloading desktop update {version}");
    let mut response = agent(Duration::from_secs(120))
        .get(&asset.browser_download_url)
        .call()?;
    let helper = update.stage.join("helper.exe");
    let mut file = File::create(&helper)?;
    let received = io::copy(
        &mut response.body_mut().as_reader().take(asset.size + 1),
        &mut file,
    )?;
    file.sync_all()?;
    drop(file);
    if received != asset.size {
        return Err("release download size mismatch".into());
    }
    verify_executable(&helper, digest(asset)?)?;
    let job = UpdateJob {
        target: target.clone(),
        parent_pid: std::process::id(),
        version,
        sha256: digest(asset)?.to_ascii_lowercase(),
        original_sha256: hash_file(&target)?,
    };
    let mut job_file = File::create(update.stage.join("update.json"))?;
    job_file.write_all(&serde_json::to_vec(&job)?)?;
    job_file.sync_all()?;
    Ok(Some(update))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn verify_executable(path: &Path, expected_hash: &str) -> Result<()> {
    if !hash_file(path)?.eq_ignore_ascii_case(expected_hash) {
        return Err("release SHA-256 mismatch".into());
    }
    // Refuse a truncated, non-PE or wrong-architecture response before running it.
    let bytes = fs::read(path)?;
    let offset = bytes
        .get(0x3c..0x40)
        .map(|value| u32::from_le_bytes(value.try_into().unwrap()) as usize)
        .ok_or("truncated executable")?;
    let header = bytes
        .get(offset..offset.saturating_add(26))
        .ok_or("invalid PE offset")?;
    if !bytes.starts_with(b"MZ")
        || &header[..4] != b"PE\0\0"
        || header[4..6] != [0x64, 0x86]
        || header[24..26] != [0x0b, 0x02]
    {
        return Err("release is not a Windows x64 executable".into());
    }
    Ok(())
}

fn spawn(executable: &Path, arguments: &[&str]) -> io::Result<Child> {
    // Native argument passing handles spaces, Unicode and shell metacharacters.
    ProcessCommand::new(executable)
        .args(arguments)
        .current_dir(executable.parent().unwrap_or_else(|| Path::new(".")))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

struct ParentProcess(HANDLE);
impl Drop for ParentProcess {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_parent(pid: u32) -> io::Result<ParentProcess> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(ParentProcess(handle))
}

fn replace(target: &Path, replacement: &Path, backup: Option<&Path>) -> io::Result<()> {
    let target = wide(target);
    let replacement = wide(replacement);
    let backup = backup.map(wide);
    let status = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            replacement.as_ptr(),
            backup.as_ref().map_or(ptr::null(), |path| path.as_ptr()),
            0,
            ptr::null(),
            ptr::null(),
        )
    };
    if status == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Keep the old binary until restart succeeds; restore it if starting fails.
fn install_and_restart(
    target: &Path,
    replacement: &Path,
    backup: &Path,
    restart: impl FnOnce() -> io::Result<()>,
) -> Result<()> {
    if let Err(error) = replace(target, replacement, Some(backup)) {
        // ReplaceFileW can report a partial move with the original in backup.
        if !target.exists() && backup.exists() {
            fs::rename(backup, target)?;
        }
        return Err(error.into());
    }
    if let Err(error) = restart() {
        replace(target, backup, None)?;
        return Err(
            format!("new app could not start; restored previous executable: {error}").into(),
        );
    }
    Ok(())
}

pub fn apply_update() -> Result<()> {
    let helper = fs::canonicalize(std::env::current_exe()?)?;
    let stage = helper.parent().ok_or("no helper directory")?;
    let job: UpdateJob =
        serde_json::from_reader(File::open(stage.join("update.json"))?.take(16384))?;
    if !valid_stage(stage, &job.target) || job.version != env!("CARGO_PKG_VERSION") {
        return Err("invalid update job".into());
    }
    verify_executable(&helper, &job.sha256)?;
    if hash_file(&job.target)? != job.original_sha256 {
        return Err("installed executable changed".into());
    }
    let parent = open_parent(job.parent_pid)?;
    fs::write(stage.join("ready"), b"ready")?;
    let status = unsafe { WaitForSingleObject(parent.0, 120_000) };
    if status != WAIT_OBJECT_0 {
        return Err("parent did not exit; leaving existing app untouched".into());
    }
    // Every failure after the old process exits must restart it, including
    // errors while copying or flushing the replacement (e.g. a full disk).
    let result: Result<()> = (|| {
        if hash_file(&job.target)? != job.original_sha256 {
            return Err("installed executable changed while waiting".into());
        }
        let replacement = stage.join("replacement.exe");
        fs::copy(&helper, &replacement)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&replacement)?
            .sync_all()?;
        verify_executable(&replacement, &job.sha256)?;
        install_and_restart(
            &job.target,
            &replacement,
            &stage.join("previous.exe"),
            || {
                spawn(&job.target, &["--background"])?;
                Ok(())
            },
        )
    })();
    if let Err(error) = result {
        // Avoid a restart/update/restart loop when installation fails. The next
        // normal startup tries again.
        let _ = spawn(&job.target, &["--background", "--skip-update"]);
        return Err(error);
    }
    log::info!("desktop updated to {}", job.version);
    if let Err(error) = fs::write(stage.join("complete"), b"complete") {
        log::warn!("update installed; could not record cleanup receipt: {error}");
    }
    Ok(())
}

fn valid_stage(stage: &Path, target: &Path) -> bool {
    let Some(name) = stage
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(STAGE_PREFIX))
    else {
        return false;
    };
    name.len() == 32
        && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        && stage.parent() == target.parent()
        && target
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}

fn remove_stage_files(stage: &Path) {
    // Delete only known files, never recursively delete a computed directory.
    for file in STAGE_FILES {
        let _ = fs::remove_file(stage.join(file));
    }
    let _ = fs::remove_dir(stage);
}

pub fn cleanup_completed() {
    // The helper may finish just after the new process launches.
    let _ = std::thread::Builder::new()
        .name("update-cleanup".into())
        .spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
            let Ok(target) = std::env::current_exe().and_then(fs::canonicalize) else {
                return;
            };
            let Some(parent) = target.parent() else {
                return;
            };
            let Ok(entries) = fs::read_dir(parent) else {
                return;
            };
            for entry in entries.flatten() {
                let Ok(stage) = fs::canonicalize(entry.path()) else {
                    continue;
                };
                if !valid_stage(&stage, &target) || !stage.join("complete").is_file() {
                    continue;
                }
                let Ok(file) = File::open(stage.join("update.json")) else {
                    continue;
                };
                let Ok(job) = serde_json::from_reader::<_, UpdateJob>(file.take(16384)) else {
                    continue;
                };
                if job.target == target {
                    remove_stage_files(&stage);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    fn release(version: &str) -> Release {
        Release {
            tag_name: format!("v{version}"),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: ASSET_NAME.into(),
                size: 1024,
                browser_download_url: format!(
                    "https://github.com/{REPOSITORY}/releases/download/v{version}/{ASSET_NAME}"
                ),
                digest: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        }
    }

    fn scratch() -> PathBuf {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("valolysis update ' & Ã¼ {suffix}"));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn upgrades_use_semver_and_never_downgrade() {
        assert!(select_asset(&release("0.10.0"), "0.9.0").unwrap().is_some());
        assert!(select_asset(&release("0.2.0"), "0.2.0").unwrap().is_none());
        assert!(select_asset(&release("0.2.0"), "0.3.0").unwrap().is_none());
    }

    #[test]
    fn skips_drafts_and_prereleases() {
        let mut value = release("0.3.0");
        value.draft = true;
        assert!(select_asset(&value, "0.2.0").unwrap().is_none());
        value.draft = false;
        value.prerelease = true;
        assert!(select_asset(&value, "0.2.0").unwrap().is_none());
        assert!(
            select_asset(&release("0.3.0-beta.1"), "0.2.0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_untrusted_or_incomplete_metadata() {
        let mut value = release("0.3.0");
        value.assets[0].browser_download_url = "https://example.org/app.exe".into();
        assert!(select_asset(&value, "0.2.0").is_err());
        let mut value = release("0.3.0");
        value.assets[0].digest = None;
        assert!(select_asset(&value, "0.2.0").is_err());
        value.assets[0].digest = Some("sha256:not-a-hash".into());
        assert!(select_asset(&value, "0.2.0").is_err());
        let mut value = release("0.3.0");
        value.assets[0].size = MAX_DOWNLOAD_BYTES + 1;
        assert!(select_asset(&value, "0.2.0").is_err());
        value.assets.clear();
        assert!(select_asset(&value, "0.2.0").is_err());
        assert!(select_asset(&release("broken"), "0.2.0").is_err());
    }

    #[test]
    fn rejects_corrupt_downloads_and_wrong_architecture() {
        let dir = scratch();
        let path = dir.join("app.exe");
        let mut pe = vec![0_u8; 256];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&128_u32.to_le_bytes());
        pe[128..132].copy_from_slice(b"PE\0\0");
        pe[132..134].copy_from_slice(&[0x64, 0x86]);
        pe[152..154].copy_from_slice(&[0x0b, 0x02]);
        fs::write(&path, &pe).unwrap();
        let hash = hash_file(&path).unwrap();
        verify_executable(&path, &hash).unwrap();
        assert!(verify_executable(&path, &"0".repeat(64)).is_err());
        pe[132..134].copy_from_slice(&[0x4c, 0x01]);
        fs::write(&path, &pe).unwrap();
        assert!(verify_executable(&path, &hash_file(&path).unwrap()).is_err());
        fs::write(&path, b"MZ").unwrap();
        assert!(verify_executable(&path, &hash_file(&path).unwrap()).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn replacement_keeps_a_backup_and_rolls_back_on_restart_failure() {
        let dir = scratch();
        let target = dir.join("app.exe");
        let candidate = dir.join("replacement.exe");
        let backup = dir.join("previous.exe");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        install_and_restart(&target, &candidate, &backup, || Ok(())).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        fs::remove_file(&backup).unwrap();
        fs::write(&candidate, b"next").unwrap();
        assert!(
            install_and_restart(&target, &candidate, &backup, || Err(io::Error::other(
                "cannot launch"
            )))
            .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");
        fs::remove_file(target).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn locked_installation_is_left_untouched() {
        let dir = scratch();
        let target = dir.join("app.exe");
        let candidate = dir.join("replacement.exe");
        let backup = dir.join("previous.exe");
        fs::write(&target, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        assert!(
            install_and_restart(&target, &candidate, &backup, || panic!("must not launch"))
                .is_err()
        );
        drop(lock);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        fs::remove_file(target).unwrap();
        fs::remove_file(candidate).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn cleanup_requires_an_immediate_valid_stage_directory() {
        let parent = Path::new("C:\\Apps");
        let target = parent.join("app.exe");
        assert!(valid_stage(
            &parent.join(format!("{STAGE_PREFIX}{}", "a".repeat(32))),
            &target
        ));
        assert!(!valid_stage(
            Path::new("C:\\Elsewhere\\.valolysis-update-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            &target
        ));
        assert!(!valid_stage(&parent.join(".valolysis-update-.."), &target));
        assert!(!valid_stage(&parent.join("unrelated"), &target));
    }

    #[test]
    #[ignore = "downloads and verifies the current public GitHub release"]
    fn live_release_download_verifies() {
        let prepared = prepare("0.0.0").unwrap().expect("published release");
        let job: UpdateJob =
            serde_json::from_slice(&fs::read(prepared.stage.join("update.json")).unwrap()).unwrap();
        verify_executable(&prepared.stage.join("helper.exe"), &job.sha256).unwrap();
    }
}
