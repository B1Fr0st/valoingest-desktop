//! Per-user installation. A downloaded copy installs itself to
//! `%LOCALAPPDATA%\Programs\Valolysis`, so updates and Start with Windows
//! always use a stable, writable path, and registers a Start menu shortcut and
//! an Apps & features entry. `--uninstall` reverses all of it.

use crate::{
    platform::{self, wide},
    store, tray, updater,
};
use semver::Version;
use std::{
    fs, io,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    ptr,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize, IPersistFile,
        },
        UI::Shell::{IShellLinkW, ShellLink},
    },
    core::{HSTRING, Interface},
};
use windows_sys::Win32::{
    System::Threading::CREATE_NO_WINDOW,
    UI::WindowsAndMessaging::{
        FindWindowW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_YESNO,
        MessageBoxW, PostMessageW,
    },
};

/// Set (or passed as `--portable`) to run in place without installing, as the
/// integration scripts and development builds do. Child processes inherit it.
pub const PORTABLE_ENV: &str = "VALOLYSIS_PORTABLE";
const EXE_NAME: &str = "valolysis.exe";
/// The name the README used to suggest saving the download under.
const LEGACY_EXE_NAME: &str = "valolysis-windows-x64.exe";
const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Valolysis";

pub fn portable() -> bool {
    cfg!(debug_assertions) || std::env::var_os(PORTABLE_ENV).is_some()
}

pub fn install_dir() -> PathBuf {
    store::local_app_data().join("Programs").join("Valolysis")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join(EXE_NAME)
}

fn shortcut_path() -> Option<PathBuf> {
    let roaming = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(roaming)
            .join("Microsoft\\Windows\\Start Menu\\Programs")
            .join("Valolysis.lnk"),
    )
}

pub enum Placement {
    /// Running in place by request.
    Portable,
    /// This is the installed copy.
    Installed,
    /// The installed copy is ready at this path; hand off to it and exit.
    Relocated(PathBuf),
}

/// Installs this executable unless it already is the installed copy. Never
/// replaces an installed copy with an older or identical version.
pub fn relocate() -> io::Result<Placement> {
    if portable() {
        return Ok(Placement::Portable);
    }
    let current = fs::canonicalize(std::env::current_exe()?)?;
    let target = installed_exe();
    if fs::canonicalize(&target).is_ok_and(|installed| installed == current) {
        return Ok(Placement::Installed);
    }
    let ours = Version::parse(env!("CARGO_PKG_VERSION")).ok();
    match installed_version(&target) {
        Some(theirs) if ours.is_none_or(|ours| ours <= theirs) => {
            log::info!("valolysis {theirs} is already installed; launching it");
        }
        _ => {
            fs::create_dir_all(install_dir())?;
            // Copy beside the target and rename, so a failed copy never leaves
            // a truncated installed executable.
            let staging = install_dir().join(format!("{EXE_NAME}.new"));
            fs::copy(&current, &staging)?;
            fs::OpenOptions::new()
                .write(true)
                .open(&staging)?
                .sync_all()?;
            fs::rename(&staging, &target)?;
            log::info!("installed to {}", target.display());
        }
    }
    if let Some(link) = shortcut_path()
        && let Err(error) = create_shortcut(&link, &target)
    {
        log::warn!("could not create the Start menu shortcut: {error}");
    }
    Ok(Placement::Relocated(target))
}

fn installed_version(target: &Path) -> Option<Version> {
    if !target.is_file() {
        return None;
    }
    let output = Command::new(target)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    Version::parse(text.trim().strip_prefix("valolysis ")?).ok()
}

/// Starts the installed copy with this process's arguments.
pub fn launch(target: &Path) -> io::Result<()> {
    Command::new(target)
        .args(std::env::args_os().skip(1))
        .current_dir(install_dir())
        .spawn()
        .map(drop)
}

/// Keeps the installed copy's registration current. Runs at every installed
/// startup, so the version shown in Apps & features follows automatic updates.
pub fn refresh_registration() {
    let exe = installed_exe();
    if let Err(error) = write_uninstall_entry(&exe) {
        log::warn!("could not register with Apps & features: {error}");
    }
    // A copy run from Downloads or elsewhere may own Start with Windows.
    if platform::autostart_command().is_some() && !platform::autostart_enabled() {
        match platform::set_autostart(true) {
            Ok(()) => log::info!("moved Start with Windows to the installed app"),
            Err(error) => log::warn!("could not update Start with Windows: {error}"),
        }
    }
}

fn write_uninstall_entry(exe: &Path) -> io::Result<()> {
    let exe_text = exe.display().to_string();
    let strings = [
        ("DisplayName", "Valolysis".to_owned()),
        ("DisplayVersion", env!("CARGO_PKG_VERSION").to_owned()),
        ("Publisher", "Valolysis".to_owned()),
        ("DisplayIcon", exe_text.clone()),
        ("InstallLocation", install_dir().display().to_string()),
        ("UninstallString", format!("\"{exe_text}\" --uninstall")),
        (
            "URLInfoAbout",
            "https://github.com/B1Fr0st/valolysis-desktop".to_owned(),
        ),
    ];
    for (name, value) in strings {
        platform::set_registry_string(UNINSTALL_KEY, name, &value)?;
    }
    platform::set_registry_dword(UNINSTALL_KEY, "NoModify", 1)?;
    platform::set_registry_dword(UNINSTALL_KEY, "NoRepair", 1)?;
    let kilobytes = fs::metadata(exe).map_or(0, |metadata| metadata.len().div_ceil(1024));
    platform::set_registry_dword(UNINSTALL_KEY, "EstimatedSize", kilobytes as u32)
}

fn create_shortcut(link: &Path, target: &Path) -> windows::core::Result<()> {
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| {
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell_link.SetPath(&HSTRING::from(target.as_os_str()))?;
            shell_link.SetWorkingDirectory(&HSTRING::from(install_dir().as_os_str()))?;
            shell_link.SetDescription(&HSTRING::from("Uploads VALORANT replays to Valolysis"))?;
            shell_link
                .cast::<IPersistFile>()?
                .Save(&HSTRING::from(link.as_os_str()), true)
        })();
        if initialized {
            CoUninitialize();
        }
        result
    }
}

fn message(text: &str, flags: u32) -> i32 {
    let text = wide(text);
    let title = wide("Valolysis");
    unsafe { MessageBoxW(ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags) }
}

/// Asks the running app to exit and waits until its instance lock is free.
fn stop_running_app() -> bool {
    let class = wide(tray::CLASS_NAME);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(instance) = platform::SingleInstance::acquire() {
            drop(instance);
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let hwnd = unsafe { FindWindowW(class.as_ptr(), ptr::null()) };
        if !hwnd.is_null() {
            unsafe { PostMessageW(hwnd, tray::WM_EXIT, 0, 0) };
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Removes the app, its registration and its sign-in. Replays are never
/// touched, and settings and upload history are removed only on request.
pub fn uninstall() {
    if message(
        "Remove Valolysis from this PC?\n\nYour VALORANT replays are not affected.",
        MB_YESNO | MB_ICONQUESTION,
    ) != IDYES
    {
        return;
    }
    if !stop_running_app() {
        message(
            "Valolysis is still running. Quit it from its notification-area icon, then try again.",
            MB_ICONERROR,
        );
        return;
    }
    let remove_data = message(
        "Also delete your Valolysis settings, upload history and logs?",
        MB_YESNO | MB_ICONQUESTION,
    ) == IDYES;

    let mut problems = Vec::new();
    let mut check = |what: &str, result: io::Result<()>| {
        if let Err(error) = result {
            problems.push(format!("{what}: {error}"));
        }
    };
    platform::Credentials::for_api("https://valolysis.odinnichols.dev").delete();
    if platform::autostart_command().is_some_and(|command| {
        command
            .to_ascii_lowercase()
            .contains(&installed_exe().display().to_string().to_ascii_lowercase())
    }) {
        check("Start with Windows", platform::set_autostart(false));
    }
    if let Some(link) = shortcut_path() {
        check("Start menu shortcut", remove_if_present(&link));
    }
    check(
        "Apps & features entry",
        platform::delete_registry_key(UNINSTALL_KEY),
    );
    if remove_data {
        let dir = store::app_dir();
        if dir.exists() {
            check("settings folder", fs::remove_dir_all(dir));
        }
    }
    check("app files", remove_install_dir());

    if problems.is_empty() {
        message("Valolysis was removed.", MB_ICONINFORMATION);
    } else {
        message(
            &format!(
                "Valolysis was removed, but some items could not be:\n\n{}",
                problems.join("\n")
            ),
            MB_ICONERROR,
        );
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn remove_install_dir() -> io::Result<()> {
    let dir = install_dir();
    let exe = installed_exe();
    updater::remove_all_stages(&exe);
    for name in [LEGACY_EXE_NAME, &format!("{EXE_NAME}.new")] {
        remove_if_present(&dir.join(name))?;
    }
    let running_installed = std::env::current_exe()
        .and_then(fs::canonicalize)
        .is_ok_and(|current| fs::canonicalize(&exe).is_ok_and(|installed| installed == current));
    if !running_installed {
        remove_if_present(&exe)?;
        return match fs::remove_dir(&dir) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    // A running executable cannot delete itself; a short-lived shell removes
    // it, and then the folder if empty, after this process exits.
    Command::new("cmd.exe")
        .raw_arg(format!(
            "/d /c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"",
            exe.display(),
            dir.display()
        ))
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
}
