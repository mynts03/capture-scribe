//! Only the executor thread owns/reaps the child. No blocking pipe-reader joins.
use crate::message::AppMessage;
use std::{
    io::{self, Read},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cancelled: bool,
}
fn unknown(e: impl ToString) -> AppMessage {
    AppMessage::known("processing.completion_unknown", e.to_string())
}
pub fn collect(
    command: Command,
    stop: &AtomicBool,
    line: impl FnMut(&str),
) -> Result<Output, AppMessage> {
    collect_with(command, stop, line, Duration::from_secs(6), None)
}
fn collect_with(
    mut command: Command,
    stop: &AtomicBool,
    mut line: impl FnMut(&str),
    timeout: Duration,
    test_fault: Option<&str>,
) -> Result<Output, AppMessage> {
    platform::prepare(&mut command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppMessage::known("processing.worker_failed", e.to_string()))?;
    let tree = match platform::Tree::attach(&mut child) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill();
            return Err(unknown(e));
        }
    };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let result = (|| {
        platform::nonblocking(&stdout).map_err(unknown)?;
        platform::nonblocking(&stderr).map_err(unknown)?;
        let (mut out, mut err, mut pending) = (Vec::new(), Vec::new(), Vec::new());
        let (mut out_eof, mut err_eof) = (false, false);
        let mut status = None;
        let mut stopped = None;
        let mut done = None;
        let mut killed = false;
        let mut fault = None;
        loop {
            // Observe a natural exit before applying cancellation.
            if status.is_none() {
                status = child.try_wait().map_err(unknown)?;
                if status.is_some() {
                    done = Some(Instant::now());
                }
            }
            if (stop.load(Ordering::SeqCst) || fault.is_some())
                && status.is_none()
                && stopped.is_none()
            {
                if test_fault == Some("stop-error") {
                    return Err(unknown("Injected owned-tree stop failure"));
                }
                tree.terminate(false).map_err(unknown)?;
                stopped = Some(Instant::now());
            }
            if stopped.is_some_and(|t| t.elapsed() >= Duration::from_secs(2)) && !killed {
                tree.terminate(true).map_err(unknown)?;
                killed = true;
            }
            // Successful worker exit must not leave children holding pipes open.
            if done.is_some_and(|t| t.elapsed() > Duration::from_secs(1))
                && !tree.empty().map_err(unknown)?
                && !killed
            {
                tree.terminate(true).map_err(unknown)?;
                killed = true;
            }
            if !out_eof {
                match drain(&mut stdout, &mut out, 16 * 1024 * 1024) {
                    Ok(e) => out_eof = e,
                    Err(e) => fault = Some(e),
                }
            }
            let previous = err.len();
            if !err_eof {
                match drain(&mut stderr, &mut err, 2 * 1024 * 1024) {
                    Ok(e) => err_eof = e,
                    Err(e) => fault = Some(e),
                }
            }
            pending.extend_from_slice(&err[previous..]);
            while let Some(n) = pending.iter().position(|&b| b == b'\n') {
                line(&String::from_utf8_lossy(&pending[..n]));
                pending.drain(..=n);
            }
            if status.is_some()
                && out_eof
                && err_eof
                && test_fault != Some("unconfirmed-tree")
                && tree.empty().map_err(unknown)?
            {
                if !pending.is_empty() {
                    line(&String::from_utf8_lossy(&pending));
                }
                if let Some(e) = fault {
                    return Err(unknown(e));
                }
                let status = status.unwrap();
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                    cancelled: stopped.is_some()
                        && !status.success()
                        && stop.load(Ordering::SeqCst),
                });
            }
            if stopped.or(done).is_some_and(|t| t.elapsed() >= timeout) {
                return Err(unknown(
                    "Owned worker/tree or pipe termination not confirmed within the wait limit",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() {
        let _ = tree.terminate(true);
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    result
}
// Per-tick cap prevents an always-writing child from monopolizing cancellation.
fn drain<T: Read + platform::Pipe>(
    pipe: &mut T,
    bytes: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    for _ in 0..32 {
        let mut buf = [0; 8192];
        let available = platform::available(pipe)?;
        if available == 0 {
            return Ok(false);
        }
        let n = available.min(buf.len());
        match pipe.read(&mut buf[..n]) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if bytes.len() + n > limit {
                    return Err(io::Error::other("Worker output limit exceeded"));
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{
        io,
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Child, Command},
    };
    pub trait Pipe: AsRawFd {}
    impl<T: AsRawFd> Pipe for T {}
    pub fn prepare(c: &mut Command) {
        c.process_group(0);
    }
    pub fn nonblocking(p: &impl Pipe) -> io::Result<()> {
        let fd = p.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn available(_: &impl Pipe) -> io::Result<usize> {
        Ok(8192)
    }
    pub struct Tree(i32);
    impl Tree {
        pub fn attach(c: &mut Child) -> io::Result<Self> {
            Ok(Self(c.id() as i32))
        }
        pub fn terminate(&self, force: bool) -> io::Result<()> {
            if unsafe { libc::kill(-self.0, if force { libc::SIGKILL } else { libc::SIGTERM }) }
                == 0
                || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        pub fn empty(&self) -> io::Result<bool> {
            if unsafe { libc::kill(-self.0, 0) } == 0 {
                Ok(false)
            } else {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ESRCH) {
                    Ok(true)
                } else {
                    Err(e)
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    // Small Win32 boundary; no name-based process discovery/termination.
    // Suspend before assigning the Job, so children cannot escape admission.
    use std::{
        ffi::c_void,
        io, mem,
        os::windows::{io::AsRawHandle, process::CommandExt},
        process::{Child, Command},
        ptr,
    };
    type Handle = *mut c_void;
    #[repr(C)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        id: u32,
        pid: u32,
        base: i32,
        delta: i32,
        flags: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Accounting {
        user: i64,
        kernel: i64,
        period_user: i64,
        period_kernel: i64,
        faults: u32,
        total: u32,
        active: u32,
        terminated: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(a: *const c_void, name: *const u16) -> Handle;
        fn AssignProcessToJobObject(j: Handle, p: Handle) -> i32;
        fn TerminateJobObject(j: Handle, code: u32) -> i32;
        fn QueryInformationJobObject(
            j: Handle,
            class: i32,
            out: *mut c_void,
            size: u32,
            ret: *mut u32,
        ) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
        fn Thread32First(s: Handle, e: *mut ThreadEntry) -> i32;
        fn Thread32Next(s: Handle, e: *mut ThreadEntry) -> i32;
        fn OpenThread(access: u32, inherit: i32, id: u32) -> Handle;
        fn ResumeThread(h: Handle) -> u32;
        fn PeekNamedPipe(
            h: Handle,
            b: *mut c_void,
            n: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
    }
    pub trait Pipe: AsRawHandle {}
    impl<T: AsRawHandle> Pipe for T {}
    pub fn prepare(c: &mut Command) {
        c.creation_flags(0x4 | 0x08000000);
    }
    pub fn nonblocking(_: &impl Pipe) -> io::Result<()> {
        Ok(())
    }
    pub fn available(p: &impl Pipe) -> io::Result<usize> {
        let mut n = 0;
        if unsafe {
            PeekNamedPipe(
                p.as_raw_handle(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut n,
                ptr::null_mut(),
            )
        } != 0
        {
            Ok(n as usize)
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(109) {
                Ok(8192)
            } else {
                Err(e)
            }
        }
    }
    pub struct Tree(Handle);
    impl Drop for Tree {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    impl Tree {
        pub fn attach(c: &mut Child) -> io::Result<Self> {
            unsafe {
                let job = Self(CreateJobObjectW(ptr::null(), ptr::null()));
                if job.0.is_null() {
                    return Err(io::Error::last_os_error());
                }
                if AssignProcessToJobObject(job.0, c.as_raw_handle()) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let snap = CreateToolhelp32Snapshot(4, 0);
                if snap == (-1isize as Handle) {
                    return Err(io::Error::last_os_error());
                }
                let mut e: ThreadEntry = mem::zeroed();
                e.size = mem::size_of::<ThreadEntry>() as u32;
                let mut found = false;
                let mut ok = Thread32First(snap, &mut e) != 0;
                while ok {
                    if e.pid == c.id() {
                        let t = OpenThread(2, 0, e.id);
                        if !t.is_null() {
                            found = ResumeThread(t) != u32::MAX;
                            CloseHandle(t);
                        }
                        break;
                    }
                    ok = Thread32Next(snap, &mut e) != 0;
                }
                CloseHandle(snap);
                if !found {
                    let _ = job.terminate(true);
                    return Err(io::Error::other(
                        "Could not resume owned worker in Job Object",
                    ));
                }
                Ok(job)
            }
        }
        pub fn terminate(&self, _force: bool) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.0, 1) } != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        pub fn empty(&self) -> io::Result<bool> {
            let mut a = Accounting::default();
            if unsafe {
                QueryInformationJobObject(
                    self.0,
                    1,
                    &mut a as *mut _ as *mut c_void,
                    mem::size_of::<Accounting>() as u32,
                    ptr::null_mut(),
                )
            } != 0
            {
                Ok(a.active == 0)
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn failed_or_unconfirmed_stop_is_unknown_not_cancelled_and_has_a_wait_bound() {
        for fault in ["stop-error", "unconfirmed-tree"] {
            let mut command = Command::new("/usr/bin/python3");
            command.args(["-c", "import time;time.sleep(20)"]);
            let start = Instant::now();
            let error = collect_with(
                command,
                &AtomicBool::new(true),
                |_| {},
                Duration::from_millis(180),
                Some(fault),
            )
            .err()
            .unwrap();
            assert_eq!(error.code.as_deref(), Some("processing.completion_unknown"));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }
    #[test]
    fn ordinary_python_descendants_inherit_the_owned_group() {
        let mut command = Command::new("/usr/bin/python3");
        command.args([
            "-c",
            "import subprocess,time;subprocess.Popen(['/bin/sleep','20']);time.sleep(20)",
        ]);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            flag.store(true, Ordering::SeqCst);
        });
        let result = collect(command, &stop, |_| {});
        t.join().unwrap();
        assert!(result.unwrap().cancelled);
    }
    #[test]
    fn natural_completion_precedes_stop_and_pipes_are_drained() {
        let mut c = Command::new("/usr/bin/python3");
        c.args(["-c", "print('fixture')"]);
        let o = collect(c, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(o.status.success());
        assert!(!o.cancelled);
        assert_eq!(o.stdout, b"fixture\n");
    }
    #[test]
    fn owns_only_spawned_tree_and_stop_is_bounded() {
        let mut unrelated = Command::new("/bin/sleep").arg("20").spawn().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let mut c = Command::new("/usr/bin/python3");
        c.args(["-c","import subprocess,time,signal; p=subprocess.Popen(['/bin/sleep','20']); print(p.pid,flush=True); signal.signal(signal.SIGTERM,lambda *_: (p.terminate(),p.wait(),exit(1))); time.sleep(20)"]);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            s.store(true, Ordering::SeqCst);
        });
        let now = Instant::now();
        let result = collect(c, &stop, |_| {});
        t.join().unwrap();
        let still_alive = unrelated.try_wait().unwrap().is_none();
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        assert!(still_alive);
        assert!(now.elapsed() < Duration::from_secs(8));
        assert!(result.unwrap().cancelled);
    }
}
