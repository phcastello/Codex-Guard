use anyhow::{bail, Context, Result};
use std::{
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
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
