use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub struct ProcessSupervisor {
    child: Child,
    #[cfg(windows)]
    job: windows::Job,
    pid: u32,
    executable: PathBuf,
    arguments: Vec<&'static str>,
}

pub fn app_server_arguments() -> Vec<&'static str> {
    vec!["--yolo", "app-server", "--stdio"]
}

fn app_server_command(executable: &Path, session_workspace: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args(app_server_arguments())
        .current_dir(session_workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false);
    command
}

impl ProcessSupervisor {
    pub fn spawn(session_workspace: &Path) -> Result<(Self, ChildStdin, ChildStdout, ChildStderr)> {
        #[cfg(windows)]
        let executable = windows::find_codex_executable()?;
        #[cfg(unix)]
        let executable =
            std::env::var_os("CODEX_GUARD_CODEX_PATH").unwrap_or_else(|| "codex".into());
        let mut command = app_server_command(Path::new(&executable), session_workspace);
        let arguments = app_server_arguments();
        #[cfg(unix)]
        unix::configure(&mut command);
        #[cfg(windows)]
        windows::configure(&mut command);
        let mut child = command.spawn().with_context(|| {
            format!(
                "spawn Codex App Server using {}",
                std::path::Path::new(&executable).display()
            )
        })?;
        let pid = child.id().context("child PID unavailable")?;
        #[cfg(windows)]
        let job = match windows::Job::assign(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let stdin = child.stdin.take().context("child stdin unavailable")?;
        let stdout = child.stdout.take().context("child stdout unavailable")?;
        let stderr = child.stderr.take().context("child stderr unavailable")?;
        Ok((
            Self {
                child,
                #[cfg(windows)]
                job,
                pid,
                executable: PathBuf::from(executable),
                arguments,
            },
            stdin,
            stdout,
            stderr,
        ))
    }
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }
    pub fn arguments(&self) -> &[&'static str] {
        &self.arguments
    }
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
    pub async fn graceful_terminate(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            unix::signal_group(self.pid, libc::SIGTERM)?;
        }
        #[cfg(windows)]
        {
            self.job.ctrl_break(self.pid)?;
        }
        Ok(())
    }
    pub async fn force_kill_tree(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            unix::signal_group(self.pid, libc::SIGKILL)?;
        }
        #[cfg(windows)]
        {
            self.job.terminate()?;
        }
        let _ = self.child.wait().await;
        Ok(())
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = unix::signal_group(self.pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_server_uses_user_workspace_regardless_of_install_directory() {
        for (executable, cwd) in [
            (
                r"C:\Users\Pedro\AppData\Local\CodexGuard\bin\codex-guard.exe",
                r"C:\Projects\Aegis",
            ),
            (
                "/home/pedro/.local/bin/codex-guard",
                "/home/pedro/Projects/Aegis",
            ),
        ] {
            // Inspect the actual spawn builder without starting any child.
            let command = app_server_command(Path::new(executable), Path::new(cwd));
            assert_eq!(command.as_std().get_current_dir(), Some(Path::new(cwd)));
            assert_eq!(
                command.as_std().get_args().collect::<Vec<_>>(),
                app_server_arguments()
            );
        }
    }
}
