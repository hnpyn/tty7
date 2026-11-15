use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const REAP_POLL: Duration = Duration::from_millis(25);

pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn output_within(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = std::thread::spawn({
        let pipe = child.stdout.take();
        move || drain(pipe)
    });
    let err = std::thread::spawn({
        let pipe = child.stderr.take();
        move || drain(pipe)
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} did not finish within {timeout:?}",
                    cmd.get_program().to_string_lossy()
                ),
            ));
        }
        std::thread::sleep(REAP_POLL);
    };

    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Starts a command nobody will read back from — an opener like `open` or
/// `xdg-open` — and reaps it once it exits. Dropping a `Child` never waits on
/// it, so each fire-and-forget spawn would otherwise sit as a zombie under
/// this process until it quits.
pub fn spawn_reaped(cmd: &mut Command) -> io::Result<u32> {
    let mut child = cmd.spawn()?;
    let pid = child.id();
    std::thread::Builder::new()
        .name("reap-child".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(pid)
}

/// Whether `pid` still names a process.
///
/// `0` is not a process: on Unix `kill(0, …)` signals the caller's process
/// group. A permission error counts as alive, so a sweep will not delete
/// files that belong to a process this one cannot signal.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    pid_alive_os(pid)
}

#[cfg(unix)]
fn pid_alive_os(pid: u32) -> bool {
    // `pid_t` is `i32`. A value that does not fit would become negative, and
    // `kill(-1, 0)` signals every process this one may signal.
    if pid > i32::MAX as u32 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn pid_alive_os(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
fn pid_alive_os(_pid: u32) -> bool {
    true
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_finishes_hands_back_its_output() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let out = output_within(&mut cmd, Duration::from_secs(10)).expect("echo runs");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[test]
    fn a_command_that_hangs_is_killed_and_reported_as_timed_out() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let err = output_within(&mut cmd, Duration::from_millis(200))
            .expect_err("sleep 30 must not finish");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the call should return as soon as the deadline passes, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_reaped_spawn_leaves_no_zombie_behind() {
        let pid = spawn_reaped(&mut Command::new("true")).expect("true runs");
        // WNOWAIT only peeks, so this never collects the child itself: the
        // pid stops being ours (ECHILD) only once the reaper has waited on it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            assert!(Instant::now() < deadline, "the child was never reaped");
            std::thread::sleep(REAP_POLL);
        }
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 400000 /dev/zero | tr '\\0' 'x'"]);
        let out = output_within(&mut cmd, Duration::from_secs(20)).expect("sh runs");
        assert_eq!(out.stdout.len(), 400_000);
    }
}
