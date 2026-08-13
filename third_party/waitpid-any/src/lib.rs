//! Wait for an arbitrary process to exit.
//!
//! The upstream crate uses Linux `pidfd` and BSD `kqueue`. Android exposes a
//! Unix target triple but does not provide the upstream crate's Linux module,
//! so Android uses the portable `kill(pid, 0)` fallback below.

use std::io::Result;
use std::time::Duration;

#[cfg(target_os = "android")]
mod imp {
    use std::io::{Error, ErrorKind, Result};
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    pub struct WaitHandle {
        pid: libc::pid_t,
        exited: bool,
    }

    pub fn open(pid: i32) -> Result<WaitHandle> {
        if pid <= 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid PID {pid}"),
            ));
        }

        // `kill(pid, 0)` checks process existence without sending a signal.
        // EPERM still means that the process exists but is not inspectable.
        let result = unsafe { libc::kill(pid, 0) };
        if result != 0 {
            let error = Error::last_os_error();
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(error);
            }
        }

        Ok(WaitHandle {
            pid,
            exited: false,
        })
    }

    fn is_alive(pid: libc::pid_t) -> Result<bool> {
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Ok(true);
        }

        let error = Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error),
        }
    }

    pub fn wait(handle: &mut WaitHandle, timeout: Option<Duration>) -> Result<Option<()>> {
        if handle.exited {
            return Ok(Some(()));
        }

        let started = Instant::now();
        loop {
            if !is_alive(handle.pid)? {
                handle.exited = true;
                return Ok(Some(()));
            }

            let sleep_for = match timeout {
                Some(limit) => {
                    let elapsed = started.elapsed();
                    if elapsed >= limit {
                        return Ok(None);
                    }
                    (limit - elapsed).min(Duration::from_millis(10))
                }
                None => Duration::from_millis(10),
            };
            std::thread::sleep(sleep_for);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io::{Error, ErrorKind, Result};
    use std::time::Duration;

    use rustix::event::{poll, PollFd, PollFlags};
    use rustix::io::Errno;
    use rustix::process::{pidfd_open, Pid, PidfdFlags};

    pub type WaitHandle = rustix::fd::OwnedFd;

    pub fn open(pid: i32) -> Result<WaitHandle> {
        let pid = Pid::from_raw(pid)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("invalid PID {pid}")))?;
        Ok(pidfd_open(pid, PidfdFlags::empty())?)
    }

    pub fn wait(pidfd: &mut WaitHandle, timeout: Option<Duration>) -> Result<Option<()>> {
        let timespec = match timeout {
            Some(duration) => Some(duration.try_into().map_err(|_| Errno::INVAL)?),
            None => None,
        };
        let mut fds = [PollFd::new(&*pidfd, PollFlags::IN)];
        let ret = poll(&mut fds, timespec.as_ref())?;
        if ret == 0 {
            return Ok(None);
        }
        Ok(Some(()))
    }
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
mod imp {
    use std::io::{Error, ErrorKind, Result};
    use std::mem::MaybeUninit;
    use std::time::Duration;

    use rustix::event::kqueue::{
        kevent, kqueue, Event, EventFilter, EventFlags, ProcessEvents,
    };
    use rustix::process::Pid;

    #[derive(Debug)]
    pub enum WaitHandle {
        KQueue(rustix::fd::OwnedFd),
        Exited,
    }

    pub fn open(pid: i32) -> Result<WaitHandle> {
        let pid = Pid::from_raw(pid)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("invalid PID {pid}")))?;
        let kqueue = kqueue()?;
        let event = Event::new(
            EventFilter::Proc {
                pid,
                flags: ProcessEvents::EXIT,
            },
            EventFlags::ADD,
            std::ptr::null_mut(),
        );
        let _ = unsafe { kevent::<_, &mut [Event; 0]>(&kqueue, &[event], &mut [], None)? };
        Ok(WaitHandle::KQueue(kqueue))
    }

    pub fn wait(handle: &mut WaitHandle, timeout: Option<Duration>) -> Result<Option<()>> {
        let kqueue = match handle {
            WaitHandle::KQueue(kqueue) => kqueue,
            WaitHandle::Exited => return Ok(Some(())),
        };
        let mut buf = [MaybeUninit::uninit()];
        let (events, _) = unsafe { kevent(&kqueue, &[], &mut buf, timeout)? };
        if events.is_empty() {
            return Ok(None);
        }
        *handle = WaitHandle::Exited;
        Ok(Some(()))
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io::{Error, Result};
    use std::ptr::NonNull;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
    };

    #[derive(Debug)]
    pub struct WaitHandle(NonNull<c_void>);

    // SAFETY: a process handle can be transferred between threads and wait()
    // takes an exclusive reference.
    unsafe impl Send for WaitHandle {}

    impl Drop for WaitHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0.as_ptr()) };
        }
    }

    pub fn open(pid: i32) -> Result<WaitHandle> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid as u32) };
        let handle = NonNull::new(handle).ok_or_else(Error::last_os_error)?;
        Ok(WaitHandle(handle))
    }

    pub fn wait(handle: &mut WaitHandle, timeout: Option<Duration>) -> Result<Option<()>> {
        let timeout = match timeout {
            Some(duration) => duration
                .as_millis()
                .try_into()
                .unwrap_or(INFINITE - 1)
                .min(INFINITE - 1),
            None => INFINITE,
        };
        match unsafe { WaitForSingleObject(handle.0.as_ptr(), timeout) } {
            WAIT_OBJECT_0 => Ok(Some(())),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(Error::last_os_error()),
        }
    }
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    windows,
)))]
mod imp {
    use std::io::{Error, ErrorKind, Result};
    use std::time::Duration;

    #[derive(Debug)]
    pub struct WaitHandle;

    pub fn open(_pid: i32) -> Result<WaitHandle> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "waiting for arbitrary processes is unsupported on this platform",
        ))
    }

    pub fn wait(_handle: &mut WaitHandle, _timeout: Option<Duration>) -> Result<Option<()>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "waiting for arbitrary processes is unsupported on this platform",
        ))
    }
}

/// A handle to a process that can be waited on even when it is not a child.
#[derive(Debug)]
#[must_use = "WaitHandle does nothing unless wait() is called"]
pub struct WaitHandle(imp::WaitHandle);

impl WaitHandle {
    /// Open a handle to the process with the given PID.
    pub fn open(pid: i32) -> Result<Self> {
        Ok(Self(imp::open(pid)?))
    }

    /// Block until the target process exits.
    pub fn wait(&mut self) -> Result<()> {
        imp::wait(&mut self.0, None)?.expect("wait without timeout cannot time out");
        Ok(())
    }

    /// Wait until the target exits or `timeout` elapses.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<()>> {
        imp::wait(&mut self.0, Some(timeout))
    }
}
