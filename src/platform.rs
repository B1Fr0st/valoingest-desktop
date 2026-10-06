//! Windows services: Credential Manager, the per-user Run key, the shell, and
//! a single-instance mutex.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, ERROR_NOT_FOUND, GetLastError},
    Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    },
    System::{
        Registry::{
            HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
            RegSetKeyValueW,
        },
        Threading::CreateMutexW,
    },
    UI::{
        Shell::{
            FO_DELETE, FOF_ALLOWUNDO, FOF_NO_UI, SHFILEOPSTRUCTW, SHFileOperationW, ShellExecuteW,
        },
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Copies `text` into a fixed, NUL-terminated UTF-16 buffer, truncating.
pub fn fill(buffer: &mut [u16], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().take(buffer.len() - 1).collect();
    buffer[..encoded.len()].copy_from_slice(&encoded);
    buffer[encoded.len()] = 0;
}

/// Returns false when another instance already holds the mutex.
pub fn acquire_single_instance() -> bool {
    let name = wide("Local\\ValolysisDesktop");
    // The handle is intentionally kept for the life of the process.
    let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    !handle.is_null() && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS
}

pub fn open(target: &str) {
    let operation = wide("open");
    let file = wide(target);
    unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn open_folder(path: &Path) {
    open(&path.to_string_lossy());
}

/// Moves a file to the Recycle Bin without any UI, so it can be restored.
pub fn recycle(path: &Path) -> std::io::Result<()> {
    // pFrom is a list of paths ending with an extra NUL.
    let mut from = wide(path.as_os_str());
    from.push(0);
    let mut operation = SHFILEOPSTRUCTW {
        hwnd: ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NO_UI) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: ptr::null_mut(),
        lpszProgressTitle: ptr::null(),
    };
    let status = unsafe { SHFileOperationW(&mut operation) };
    if status != 0 || operation.fAnyOperationsAborted != 0 || path.exists() {
        return Err(std::io::Error::other(format!(
            "could not move to the Recycle Bin (code {status})"
        )));
    }
    Ok(())
}

/// The Valolysis session token in Windows Credential Manager, per API host.
pub struct Credentials {
    target: Vec<u16>,
}

impl Credentials {
    pub fn for_api(api: &str) -> Self {
        let host = api
            .split("://")
            .nth(1)
            .unwrap_or(api)
            .split('/')
            .next()
            .unwrap_or(api);
        Self {
            target: wide(format!("valolysis:{host}")),
        }
    }

    pub fn read(&self) -> Option<String> {
        let mut credential: *mut CREDENTIALW = ptr::null_mut();
        if unsafe { CredReadW(self.target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
            return None;
        }
        let token = unsafe {
            let value = &*credential;
            let blob =
                std::slice::from_raw_parts(value.CredentialBlob, value.CredentialBlobSize as usize);
            String::from_utf8(blob.to_vec()).ok()
        };
        unsafe { CredFree(credential.cast()) };
        token
    }

    pub fn write(&self, token: &str) -> std::io::Result<()> {
        let mut target = self.target.clone();
        let mut user = wide("session");
        let mut blob = token.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            UserName: user.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn delete(&self) {
        if unsafe { CredDeleteW(self.target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0
            && unsafe { GetLastError() } != ERROR_NOT_FOUND
        {
            log::warn!(
                "could not delete stored session: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Valolysis";

fn run_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --background", exe.display()))
}

/// True when the Run key points at this executable.
pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    let mut buffer = [0_u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return false;
    }
    let stored = String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)]);
    run_command().is_some_and(|command| command.eq_ignore_ascii_case(&stored))
}

pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let key = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    let status = if enabled {
        let command = wide(
            run_command().ok_or_else(|| std::io::Error::other("executable path unavailable"))?,
        );
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        }
    } else {
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
        if status == 2 { 0 } else { status } // ERROR_FILE_NOT_FOUND: already off
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(status as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_truncates_and_terminates() {
        let mut buffer = [1_u16; 4];
        fill(&mut buffer, "abcdef");
        assert_eq!(buffer, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    /// Moves a scratch file to the real Recycle Bin, so it is opt-in:
    /// `cargo test -- --ignored recycle`.
    #[test]
    #[ignore]
    fn recycle_moves_a_file_to_the_recycle_bin() {
        let path =
            std::env::temp_dir().join(format!("valolysis-recycle-test-{}.vrf", std::process::id()));
        std::fs::write(&path, b"test").unwrap();
        recycle(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn credential_target_uses_host() {
        let credentials = Credentials::for_api("https://valolysis.example.test/");
        assert_eq!(
            String::from_utf16_lossy(&credentials.target),
            "valolysis:valolysis.example.test\0"
        );
    }
}
