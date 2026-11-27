use std::io;
use std::process::Command;

#[cfg(all(windows, test))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(all(test, unix))]
const REAP_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Hide a console window for a short-lived helper. Tests spawn `ping` and
/// `cmd` on Windows; a flashing console is not part of the assertion.
#[cfg(test)]
pub(crate) fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
}
