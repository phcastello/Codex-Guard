//! Installation is invoked only by normal CLI startup, never by build or tests.
//! All filesystem operations accept explicit paths; persistent PATH writes live
//! exclusively in the platform backends.
use anyhow::{Context, Result};
use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

#[cfg(any(unix, test))]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod windows_path;

const REEXEC_MARKER: &str = "_CODEX_GUARD_INSTALLED_REEXEC";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub struct ProcessContext {
    session_workspace: PathBuf,
    executable_path: PathBuf,
    arguments: Vec<OsString>,
}

impl ProcessContext {
    pub fn capture() -> Result<Self> {
        // Capture cwd first. It never comes from current_exe or InstallPaths.
        let session_workspace = env::current_dir().context("capture session workspace")?;
        Ok(Self {
            session_workspace,
            executable_path: env::current_exe().context("locate Guard executable")?,
            arguments: env::args_os().skip(1).collect(),
        })
    }
    pub fn workspace(&self) -> &Path {
        &self.session_workspace
    }
    pub fn executable(&self) -> &Path {
        &self.executable_path
    }
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Debug, Clone)]
pub struct InstallPaths {
    pub bin_dir: PathBuf,
    pub executable: PathBuf,
    pub alias: PathBuf,
}

impl InstallPaths {
    #[cfg(any(windows, test))]
    pub fn windows(local_app_data: &Path) -> Self {
        let bin_dir = local_app_data.join("CodexGuard").join("bin");
        Self {
            executable: bin_dir.join("codex-guard.exe"),
            alias: bin_dir.join("cg.exe"),
            bin_dir,
        }
    }
    #[cfg(any(unix, test))]
    pub fn linux(home: &Path) -> Self {
        let bin_dir = home.join(".local").join("bin");
        Self {
            executable: bin_dir.join("codex-guard"),
            alias: bin_dir.join("cg"),
            bin_dir,
        }
    }
    pub fn discover() -> Result<Self> {
        #[cfg(windows)]
        let paths = Self::windows(&PathBuf::from(
            env::var_os("LOCALAPPDATA")
                .filter(|v| !v.is_empty())
                .context("LOCALAPPDATA is unavailable")?,
        ));
        #[cfg(unix)]
        let paths = Self::linux(&PathBuf::from(
            env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .context("HOME is unavailable")?,
        ));
        anyhow::ensure!(
            paths.bin_dir.is_absolute(),
            "installation directory must be absolute"
        );
        Ok(paths)
    }
}

pub fn platform() -> String {
    format!("{}-{}", env::consts::OS, env::consts::ARCH)
}

fn same_location(a: &Path, b: &Path) -> bool {
    let a = fs::canonicalize(a).unwrap_or_else(|_| a.to_owned());
    let b = fs::canonicalize(b).unwrap_or_else(|_| b.to_owned());
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(unix)]
    {
        a == b
    }
}

pub fn is_running_from_install_location(executable: &Path, paths: &InstallPaths) -> bool {
    same_location(executable, &paths.executable) || same_location(executable, &paths.alias)
}

#[derive(Debug)]
pub enum InstallOutcome {
    AlreadyInstalled,
    Installed {
        path_message: String,
        warnings: Vec<String>,
    },
}

pub(super) enum PathOutcome {
    Ready(String),
    #[cfg(unix)]
    Manual(String),
}

// Stage before replacing so a locked target, interrupted copy or permission error
// cannot truncate the previous installation. Staging stays on the same filesystem.
pub(super) fn copy_binary(source: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .context("installation target has no parent")?;
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(
        ".codex-guard-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut staged_created = false;
    let result = (|| -> Result<()> {
        let mut input = fs::File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        staged_created = true;
        std::io::copy(&mut input, &mut output)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        output.sync_all()?;
        drop(output);
        #[cfg(windows)]
        windows::replace_file(&staged, target)?;
        #[cfg(unix)]
        fs::rename(&staged, target)?;
        Ok(())
    })();
    if result.is_err() && staged_created {
        let _ = fs::remove_file(&staged);
    }
    result.with_context(|| format!("install {}", target.display()))
}

pub fn ensure_installed(context: &ProcessContext, paths: &InstallPaths) -> Result<InstallOutcome> {
    if is_running_from_install_location(context.executable(), paths) {
        return Ok(InstallOutcome::AlreadyInstalled);
    }
    copy_binary(context.executable(), &paths.executable)?;
    let mut warnings = Vec::new();
    #[cfg(windows)]
    let alias_result = copy_binary(context.executable(), &paths.alias);
    #[cfg(unix)]
    let alias_result = linux::install_alias(paths);
    if let Err(error) = alias_result {
        warnings.push(format!("Could not install cg: {error:#}"));
    }
    #[cfg(windows)]
    let path_result = windows::ensure_user_path(&paths.bin_dir);
    #[cfg(unix)]
    let path_result = linux::ensure_user_path(&paths.bin_dir);
    let path_message = match path_result {
        Ok(PathOutcome::Ready(message)) => message,
        #[cfg(unix)]
        Ok(PathOutcome::Manual(message)) => {
            warnings.push(message);
            String::new()
        }
        Err(error) => {
            warnings.push(format!("Automatic PATH configuration failed: {error:#}. Add {} to your user PATH; cg may not be available globally.", paths.bin_dir.display()));
            String::new()
        }
    };
    Ok(InstallOutcome::Installed {
        path_message,
        warnings,
    })
}

// Pure decision, also tested without installing or launching anything.
fn needs_install(executable: &Path, paths: &InstallPaths, reexecuted: bool) -> bool {
    !reexecuted && !is_running_from_install_location(executable, paths)
}

pub fn reexec_command(context: &ProcessContext, paths: &InstallPaths) -> Result<Command> {
    let mut command = Command::new(&paths.executable);
    command
        .args(context.arguments())
        .current_dir(context.workspace());
    let inherited = env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = env::split_paths(&inherited).collect();
    if !dirs.iter().any(|dir| same_location(dir, &paths.bin_dir)) {
        dirs.insert(0, paths.bin_dir.clone());
    }
    command
        .env("PATH", env::join_paths(dirs)?)
        .env(REEXEC_MARKER, "1");
    Ok(command)
}

/// Returns a child status only on Windows. Unix replaces this process with exec.
/// Installation/exec failures fall back to the downloaded executable.
pub fn startup(
    context: &ProcessContext,
    paths: Option<&InstallPaths>,
) -> Result<Option<ExitStatus>> {
    let reexecuted = env::var_os(REEXEC_MARKER).is_some();
    // main calls this before creating runtime threads. Do not leak the internal
    // marker to Codex or other child processes.
    env::remove_var(REEXEC_MARKER);
    let Some(paths) = paths else {
        eprintln!("Automatic installation failed: user installation directory unavailable. You can continue using this executable, but cg will not be available globally.");
        return Ok(None);
    };
    if !needs_install(context.executable(), paths, reexecuted) {
        return Ok(None);
    }
    match ensure_installed(context, paths) {
        Ok(InstallOutcome::AlreadyInstalled) => return Ok(None),
        Ok(InstallOutcome::Installed {
            path_message,
            warnings,
        }) => {
            eprintln!("Codex Guard installed to:\n{}", paths.executable.display());
            if !path_message.is_empty() {
                eprintln!("{path_message}");
            }
            for warning in &warnings {
                eprintln!("{warning}");
            }
            if warnings.is_empty() {
                eprintln!("Commands available in new terminals:\n  codex-guard\n  cg");
            }
        }
        Err(error) => {
            eprintln!("Automatic installation failed: {error:#}\nYou can continue using this executable, but cg may not be available globally.");
            return Ok(None);
        }
    }
    let result = reexec_command(context, paths).and_then(|mut command| {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Err(command.exec().into())
        }
        #[cfg(windows)]
        {
            // Keep the invoking terminal waiting and propagate the installed
            // process's exit code. No second TUI/runtime runs in this parent.
            command.spawn().map_err(anyhow::Error::from)
        }
    });
    match result {
        #[cfg(windows)]
        Ok(mut child) => Ok(Some(child.wait().context("wait for installed Guard")?)),
        #[cfg(unix)]
        Ok(status) => Ok(status),
        Err(error) => {
            eprintln!("Could not reexecute installed Guard: {error:#}\nContinuing with this executable in {}.", context.workspace().display());
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_locations_and_aliases() {
        let root = Path::new("user-data");
        let windows = InstallPaths::windows(root);
        assert_eq!(
            windows.executable,
            root.join("CodexGuard/bin/codex-guard.exe")
        );
        assert_eq!(windows.alias, root.join("CodexGuard/bin/cg.exe"));
        let linux = InstallPaths::linux(root);
        assert_eq!(linux.executable, root.join(".local/bin/codex-guard"));
        assert_eq!(linux.alias, root.join(".local/bin/cg"));
    }

    #[test]
    fn captures_actual_cwd_independent_of_executable() {
        assert_eq!(
            ProcessContext::capture().unwrap().workspace(),
            env::current_dir().unwrap()
        );
    }

    #[test]
    fn workspace_arguments_and_loop_guard_survive_reexec() {
        let temp = tempfile::tempdir().unwrap();
        let paths = InstallPaths::linux(&temp.path().join("home"));
        for arguments in [
            vec!["-p", "conservative", "-c", "5", "corrija isso"],
            vec!["config", "show"],
            vec!["profiles"],
            vec!["--", "a \"quote\"; $HOME"],
        ] {
            let context = ProcessContext {
                session_workspace: temp.path().join("Projects/Aegis"),
                executable_path: temp.path().join("Downloads/guard"),
                arguments: arguments.iter().map(OsString::from).collect(),
            };
            let command = reexec_command(&context, &paths).unwrap();
            assert_eq!(command.get_current_dir(), Some(context.workspace()));
            assert_ne!(context.workspace(), paths.bin_dir);
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                context.arguments.iter().collect::<Vec<_>>()
            );
            assert!(needs_install(context.executable(), &paths, false));
            assert!(!needs_install(context.executable(), &paths, true));
            assert!(!needs_install(&paths.executable, &paths, false));
            assert!(!needs_install(&paths.alias, &paths, false));
            assert!(command
                .get_envs()
                .any(|(key, value)| key == REEXEC_MARKER && value == Some("1".as_ref())));
        }
    }

    #[test]
    fn staged_copy_updates_without_removing_download_or_old_file_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("download");
        let target = temp.path().join("bin/guard");
        fs::write(&source, b"v1").unwrap();
        copy_binary(&source, &target).unwrap();
        fs::write(&source, b"v2").unwrap();
        copy_binary(&source, &target).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"v2");
        assert_eq!(fs::read(&target).unwrap(), b"v2");
        assert!(copy_binary(&temp.path().join("missing"), &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"v2");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn installed_executable_is_not_recopied() {
        let temp = tempfile::tempdir().unwrap();
        let paths = InstallPaths::linux(temp.path());
        for executable in [&paths.executable, &paths.alias] {
            let context = ProcessContext {
                session_workspace: temp.path().join("project"),
                executable_path: executable.clone(),
                arguments: vec![],
            };
            assert!(matches!(
                ensure_installed(&context, &paths).unwrap(),
                InstallOutcome::AlreadyInstalled
            ));
        }
        assert!(!paths.bin_dir.exists());
    }

    #[cfg(windows)]
    #[test]
    fn locked_windows_installation_is_preserved_on_update_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("download.exe");
        let target = temp.path().join("installed.exe");
        fs::write(&source, b"new version").unwrap();
        fs::write(&target, b"old version").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        assert!(copy_binary(&source, &target).is_err());
        drop(lock);
        assert_eq!(fs::read(&target).unwrap(), b"old version");
        assert_eq!(fs::read(&source).unwrap(), b"new version");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }
}
