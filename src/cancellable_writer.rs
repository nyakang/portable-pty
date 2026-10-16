use filedescriptor::FileDescriptor;
use std::io::{self, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Default)]
struct State {
    stopped: AtomicBool,
    #[cfg(windows)]
    thread: std::sync::Mutex<Option<std::os::windows::io::OwnedHandle>>,
}

/// A cancellation handle may be retried while a writer is finishing. This
/// covers cancellation racing the entry to a Windows synchronous system call.
#[derive(Clone)]
pub struct WriteCancellation(Arc<State>);

impl WriteCancellation {
    pub fn cancel(&self) {
        self.0.stopped.store(true, Ordering::Release);
        #[cfg(windows)]
        if let Ok(thread) = self.0.thread.lock() {
            use std::os::windows::io::AsRawHandle;
            if let Some(thread) = thread.as_ref() {
                unsafe {
                    winapi::um::ioapiset::CancelSynchronousIo(thread.as_raw_handle().cast());
                }
            }
        }
    }
}

/// No EOF bytes are written from Drop: cancellation must never initiate I/O.
pub struct CancellableWriter {
    fd: FileDescriptor,
    state: Arc<State>,
}

impl CancellableWriter {
    pub(crate) fn new(fd: FileDescriptor) -> anyhow::Result<(Self, WriteCancellation)> {
        #[cfg(unix)]
        let mut fd = fd;
        #[cfg(unix)]
        fd.set_non_blocking(true)?;
        let state = Arc::new(State::default());
        Ok((
            Self {
                fd,
                state: Arc::clone(&state),
            },
            WriteCancellation(state),
        ))
    }

    fn check_stopped(&self) -> io::Result<()> {
        if self.state.stopped.load(Ordering::Acquire) {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY writer cancelled",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(windows)]
    fn operation(
        &mut self,
        run: impl FnOnce(&mut FileDescriptor) -> io::Result<usize>,
    ) -> io::Result<usize> {
        use std::os::windows::io::BorrowedHandle;
        let thread = unsafe {
            BorrowedHandle::borrow_raw(winapi::um::processthreadsapi::GetCurrentThread().cast())
        }
        .try_clone_to_owned()?;
        *self
            .state
            .thread
            .lock()
            .map_err(|_| io::Error::other("PTY write lock poisoned"))? = Some(thread);
        let result = self.check_stopped().and_then(|()| run(&mut self.fd));
        if let Ok(mut thread) = self.state.thread.lock() {
            *thread = None;
        }
        result
    }
}

impl Write for CancellableWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(windows)]
        {
            self.operation(|fd| fd.write(bytes))
        }
        #[cfg(unix)]
        loop {
            self.check_stopped()?;
            match self.fd.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    use std::os::fd::AsRawFd;
                    let mut poll = libc::pollfd {
                        fd: self.fd.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut poll, 1, 25) } < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check_stopped()?;
        // PTYs are unbuffered. FlushFileBuffers on a ConPTY input pipe waits
        // for the peer to read, so it must not be used as an application flush.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::CancellableWriter;
    use filedescriptor::Pipe;
    use std::{io::Write, sync::mpsc, time::Duration};

    #[test]
    fn cancellation_releases_a_writer_with_a_full_pipe() {
        let pipe = Pipe::new().unwrap();
        let (mut writer, cancel) = CancellableWriter::new(pipe.write).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = writer.write_all(&vec![0; 4 * 1024 * 1024]);
            done_tx.send(result.is_err()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        for _ in 0..100 {
            cancel.cancel();
            if let Ok(failed) = done_rx.recv_timeout(Duration::from_millis(10)) {
                assert!(failed);
                worker.join().unwrap();
                drop(pipe.read);
                return;
            }
        }
        panic!("cancelled pipe write did not finish");
    }
}
