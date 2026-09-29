use anyhow::{Context, Result};
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
}

impl ProcessSupervisor {
    pub fn spawn() -> Result<(Self, ChildStdin, ChildStdout, ChildStderr)> {
        let mut command = Command::new("codex");
        command
            .arg("app-server")
            .arg("--stdio")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false);
        #[cfg(unix)]
        unix::configure(&mut command);
        #[cfg(windows)]
        windows::configure(&mut command);
        let mut child = command.spawn().context("spawn codex app-server")?;
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
            },
            stdin,
            stdout,
            stderr,
        ))
    }
    pub fn id(&self) -> u32 {
        self.pid
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
