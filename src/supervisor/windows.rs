use anyhow::{bail, Context, Result};
use std::{
    env,
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{OpenProcess, CREATE_NEW_PROCESS_GROUP, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    },
};

pub fn configure(command: &mut Command) {
    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
}

pub fn find_codex_executable() -> Result<PathBuf> {
    if let Some(value) = env::var_os("CODEX_GUARD_CODEX_PATH").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if is_native_executable(&path) {
            return Ok(path);
        }
        bail!(
            "CODEX_GUARD_CODEX_PATH must point to an existing codex.exe: {}",
            path.display()
        );
    }

    let path_dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    let mut npm_prefixes = path_dirs.clone();
    if let Some(value) = env::var_os("npm_config_prefix") {
        npm_prefixes.push(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("APPDATA") {
        npm_prefixes.push(PathBuf::from(value).join("npm"));
    }
    find_in_locations(&path_dirs, &npm_prefixes).context(
        "Codex CLI native executable not found. Install @openai/codex or set CODEX_GUARD_CODEX_PATH to the full path of codex.exe",
    )
}

fn is_native_executable(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("exe"))
}

fn find_in_locations(path_dirs: &[PathBuf], npm_prefixes: &[PathBuf]) -> Option<PathBuf> {
    // A native installation wins over npm shims. CreateProcess cannot launch
    // codex.cmd or codex.ps1 directly, and a shell wrapper would weaken Job ownership.
    for dir in path_dirs {
        let candidate = dir.join("codex.exe");
        if is_native_executable(&candidate) {
            return Some(candidate);
        }
    }
    for prefix in npm_prefixes {
        for candidate in npm_native_executables(prefix) {
            if is_native_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn npm_native_executables(prefix: &Path) -> [PathBuf; 2] {
    #[cfg(target_arch = "x86_64")]
    let (package, target) = ("codex-win32-x64", "x86_64-pc-windows-msvc");
    #[cfg(target_arch = "aarch64")]
    let (package, target) = ("codex-win32-arm64", "aarch64-pc-windows-msvc");
    let package_root = prefix.join("node_modules").join("@openai").join("codex");
    [
        package_root
            .join("node_modules")
            .join("@openai")
            .join(package)
            .join("vendor")
            .join(target)
            .join("bin")
            .join("codex.exe"),
        package_root
            .join("vendor")
            .join(target)
            .join("bin")
            .join("codex.exe"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_native_binary_beside_windows_npm_shims_without_running_it() {
        let root = env::temp_dir().join(format!(
            "codex-guard-resolver-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let prefix = root.join("npm");
        let native = npm_native_executables(&prefix)[0].clone();
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        fs::write(&native, b"not run by this test").unwrap();
        fs::write(prefix.join("codex.cmd"), b"shim").unwrap();
        assert_eq!(
            find_in_locations(&[prefix.clone()], &[prefix]),
            Some(native)
        );
        let resolved_root = fs::canonicalize(&root).unwrap();
        let resolved_temp = fs::canonicalize(env::temp_dir()).unwrap();
        assert!(resolved_root.starts_with(resolved_temp));
        fs::remove_dir_all(resolved_root).unwrap();
    }

    #[test]
    fn ignores_cmd_shims_without_native_binary() {
        assert_eq!(find_in_locations(&[], &[]), None);
        assert!(!is_native_executable(Path::new("codex.cmd")));
    }
}

pub struct Job(HANDLE);
impl Job {
    pub fn assign(child: &Child) -> Result<Self> {
        let pid = child.id().context("child PID unavailable")?;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error()).context("CreateJobObjectW");
            }
            let owned = Self(job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error()).context("SetInformationJobObject");
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return Err(std::io::Error::last_os_error()).context("OpenProcess");
            }
            let result = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if result == 0 {
                return Err(std::io::Error::last_os_error()).context("AssignProcessToJobObject");
            }
            Ok(owned)
        }
    }
    pub fn ctrl_break(&self, pid: u32) -> Result<()> {
        if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
            bail!(
                "GenerateConsoleCtrlEvent: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }
    pub fn terminate(&self) -> Result<()> {
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error()).context("TerminateJobObject");
        }
        Ok(())
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
