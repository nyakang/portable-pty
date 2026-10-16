use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, Result as IoResult};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use winapi::shared::minwindef::DWORD;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;

pub mod conpty;
mod procthreadattr;
mod psuedocon;

pub use psuedocon::{configure_bundled_conpty, conpty_status, ConptyBackend, ConptyStatus};

use filedescriptor::OwnedHandle;

#[derive(Debug)]
pub struct WinChild {
    proc: Arc<Mutex<OwnedHandle>>,
}

impl WinChild {
    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = self
            .proc
            .lock()
            .map_err(|_| IoError::other("process handle lock poisoned"))?;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            if status == STILL_ACTIVE {
                Ok(None)
            } else {
                Ok(Some(ExitStatus::with_exit_code(status)))
            }
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn do_kill(&mut self) -> IoResult<()> {
        let proc = self
            .proc
            .lock()
            .map_err(|_| IoError::other("process handle lock poisoned"))?;
        terminate_process(&proc)
    }
}

// A process can exit between querying it and TerminateProcess. Treat that
// race as successful cleanup while preserving real query/termination errors.
fn terminate_process(proc: &OwnedHandle) -> IoResult<()> {
    let mut status = 0;
    if unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) } == 0 {
        return Err(IoError::last_os_error());
    }
    if status != STILL_ACTIVE {
        return Ok(());
    }
    if unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) } != 0 {
        return Ok(());
    }
    let error = IoError::last_os_error();
    if unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) } != 0
        && status != STILL_ACTIVE
    {
        return Ok(());
    }
    Err(error)
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        self.do_kill()
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(WinChildKiller {
            proc: Arc::clone(&self.proc),
        })
    }
}

#[derive(Debug)]
pub struct WinChildKiller {
    proc: Arc<Mutex<OwnedHandle>>,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        let proc = self
            .proc
            .lock()
            .map_err(|_| IoError::other("process handle lock poisoned"))?;
        terminate_process(&proc)
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(WinChildKiller {
            proc: Arc::clone(&self.proc),
        })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        let proc = self
            .proc
            .lock()
            .map_err(|_| IoError::other("process handle lock poisoned"))?
            .try_clone()
            .map_err(IoError::other)?;
        if unsafe { WaitForSingleObject(proc.as_raw_handle() as _, INFINITE) } == u32::MAX {
            return Err(IoError::last_os_error());
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn process_id(&self) -> Option<u32> {
        let proc = self.proc.lock().ok()?;
        let res = unsafe { GetProcessId(proc.as_raw_handle() as _) };
        if res == 0 {
            None
        } else {
            Some(res)
        }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = self.proc.lock().ok()?;
        Some(proc.as_raw_handle())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                struct PassRawHandleToWaiterThread(pub RawHandle);
                unsafe impl Send for PassRawHandleToWaiterThread {}

                let proc = self
                    .proc
                    .lock()
                    .map_err(|_| IoError::other("process handle lock poisoned"))?
                    .try_clone()?;
                let handle = PassRawHandleToWaiterThread(proc.as_raw_handle());

                let waker = cx.waker().clone();
                std::thread::Builder::new()
                    .name("pty-process-wait".into())
                    .spawn(move || {
                        unsafe {
                            WaitForSingleObject(handle.0 as _, INFINITE);
                        }
                        waker.wake();
                        drop(proc);
                    })?;
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod process_error_tests {
    use super::WinChild;
    use crate::{Child, ChildKiller};
    use filedescriptor::OwnedHandle;
    use std::os::windows::io::FromRawHandle;
    use std::sync::{Arc, Mutex};

    #[test]
    fn query_and_termination_failures_are_errors() {
        // A real process handle without query or termination rights deterministically
        // exercises access-denied paths without invalid handles or resource exhaustion.
        let handle = unsafe {
            winapi::um::processthreadsapi::OpenProcess(
                winapi::um::winnt::SYNCHRONIZE,
                0,
                winapi::um::processthreadsapi::GetCurrentProcessId(),
            )
        };
        assert!(!handle.is_null());
        let mut child = WinChild {
            proc: Arc::new(Mutex::new(unsafe {
                OwnedHandle::from_raw_handle(handle.cast())
            })),
        };
        assert!(child.try_wait().is_err());
        assert!(child.kill().is_err());
        assert!(child.clone_killer().kill().is_err());
    }

    #[test]
    fn termination_succeeds_and_is_idempotent_after_exit() {
        use std::os::windows::io::{AsHandle, IntoRawHandle};
        use std::os::windows::process::CommandExt;
        let mut process = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let handle = process
            .as_handle()
            .try_clone_to_owned()
            .unwrap()
            .into_raw_handle();
        let mut child = WinChild {
            proc: Arc::new(Mutex::new(unsafe { OwnedHandle::from_raw_handle(handle) })),
        };
        let mut killer = child.clone_killer();
        killer.kill().unwrap();
        child.wait().unwrap();
        child.kill().unwrap();
        killer.kill().unwrap();
        process.wait().unwrap();
    }

    #[test]
    fn poisoned_process_locks_do_not_panic() {
        let handle = unsafe {
            winapi::um::synchapi::CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null())
        };
        assert!(!handle.is_null());
        let proc = Arc::new(Mutex::new(unsafe {
            OwnedHandle::from_raw_handle(handle.cast())
        }));
        let poisoned = Arc::clone(&proc);
        assert!(std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("injected lock failure");
        })
        .join()
        .is_err());
        let mut child = WinChild { proc };
        assert!(child.try_wait().is_err());
        assert!(child.kill().is_err());
        assert!(child.clone_killer().kill().is_err());
    }
}
