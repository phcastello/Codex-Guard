use super::{windows_path::append_path, PathOutcome};
use anyhow::{bail, Context, Result};
use std::{
    env,
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS},
    Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH},
    System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ,
    },
    UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    },
};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

pub(super) fn replace_file(source: &Path, target: &Path) -> Result<()> {
    let source = wide(source.as_os_str());
    let target = wide(target.as_os_str());
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("replace installed binary (close existing Guard sessions before updating)");
    }
    Ok(())
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn registry_result(status: u32) -> Result<()> {
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32).into());
    }
    Ok(())
}

fn read_path(key: &Key, name: &[u16]) -> Result<(OsString, u32)> {
    let mut kind = 0;
    let mut bytes = 0;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null(),
            &mut kind,
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok((OsString::new(), REG_EXPAND_SZ));
    }
    registry_result(status)?;
    loop {
        let mut buffer = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut size = (buffer.len() * 2) as u32;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status == ERROR_MORE_DATA {
            bytes = size;
            continue;
        }
        registry_result(status)?;
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            bail!("HKCU\\Environment\\Path is not a string; it was preserved");
        }
        anyhow::ensure!(size % 2 == 0, "invalid UTF-16 User PATH; it was preserved");
        buffer.truncate(size as usize / 2);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        anyhow::ensure!(
            !buffer.contains(&0),
            "User PATH contains an embedded NUL; it was preserved"
        );
        return Ok((OsString::from_wide(&buffer), kind));
    }
}

pub(super) fn ensure_user_path(bin: &Path) -> Result<PathOutcome> {
    let subkey = wide(OsStr::new("Environment"));
    let name = wide(OsStr::new("Path"));
    let mut handle = ptr::null_mut();
    registry_result(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            ptr::null(),
            &mut handle,
            ptr::null_mut(),
        )
    })
    .context("open HKCU\\Environment")?;
    let key = Key(handle);
    let (existing, kind) = read_path(&key, &name)?;
    let local = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?;
    let Some(updated) = append_path(&existing, bin.as_os_str(), &local) else {
        return Ok(PathOutcome::Ready(String::new()));
    };
    let updated = wide(&updated);
    registry_result(unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            kind,
            updated.as_ptr().cast(),
            (updated.len() * 2).try_into()?,
        )
    })
    .context("write User PATH in HKCU\\Environment")?;
    let environment = wide(OsStr::new("Environment"));
    // Notification is best effort. The reexec's PATH is set independently.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            ptr::null_mut(),
        );
    }
    Ok(PathOutcome::Ready(
        "Added to user PATH. Open a new terminal to use cg.".into(),
    ))
}
