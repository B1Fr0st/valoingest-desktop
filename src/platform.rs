//! Windows services: Credential Manager, the per-user registry, the shell, and
//! a single-instance mutex.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, GetLastError,
        HANDLE,
    },
    Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    },
    System::{
        Registry::{
            HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW,
            RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
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

/// Held for the life of the app; dropping it lets another instance start.
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    /// Returns None when another instance already holds the mutex.
    pub fn acquire() -> Option<Self> {
        let name = wide("Local\\ValolysisDesktop");
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        let instance = Self(handle);
        (unsafe { GetLastError() } != ERROR_ALREADY_EXISTS).then_some(instance)
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
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

fn status(code: u32) -> std::io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(code as i32))
    }
}

/// Reads a string value under `HKEY_CURRENT_USER`.
pub fn registry_string(key: &str, name: &str) -> Option<String> {
    let key = wide(key);
    let name = wide(name);
    let mut buffer = [0_u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (code == 0).then(|| String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)]))
}

/// Writes a value under `HKEY_CURRENT_USER`, creating the key if needed.
fn set_registry_value(key: &str, name: &str, kind: u32, data: &[u8]) -> std::io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    status(unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            kind,
            data.as_ptr().cast(),
            data.len() as u32,
        )
    })
}

pub fn set_registry_string(key: &str, name: &str, value: &str) -> std::io::Result<()> {
    let data: Vec<u8> = wide(value)
        .iter()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    set_registry_value(key, name, REG_SZ, &data)
}

pub fn set_registry_dword(key: &str, name: &str, value: u32) -> std::io::Result<()> {
    set_registry_value(key, name, REG_DWORD, &value.to_le_bytes())
}

/// Deletes a value; succeeds when it is already gone.
pub fn delete_registry_value(key: &str, name: &str) -> std::io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    let code = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
    status(if code == ERROR_FILE_NOT_FOUND {
        0
    } else {
        code
    })
}

/// Deletes a key and everything under it; succeeds when it is already gone.
pub fn delete_registry_key(key: &str) -> std::io::Result<()> {
    let key = wide(key);
    let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
    status(if code == ERROR_FILE_NOT_FOUND {
        0
    } else {
        code
    })
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Valolysis";

fn run_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let portable = if crate::install::portable() {
        " --portable"
    } else {
        ""
    };
    Some(format!("\"{}\"{portable}", exe.display()))
}

/// The current Run key command, whichever executable it points at.
pub fn autostart_command() -> Option<String> {
    registry_string(RUN_KEY, RUN_VALUE)
}

/// True when the Run key points at this executable.
pub fn autostart_enabled() -> bool {
    autostart_command().is_some_and(|stored| {
        run_command().is_some_and(|command| command.eq_ignore_ascii_case(&stored))
    })
}

pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    if enabled {
        let command =
            run_command().ok_or_else(|| std::io::Error::other("executable path unavailable"))?;
        set_registry_string(RUN_KEY, RUN_VALUE, &command)
    } else {
        delete_registry_value(RUN_KEY, RUN_VALUE)
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
