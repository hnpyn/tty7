//! Foreground executable name on macOS and Linux.
//!
//! `pane::foreground_title` reads it for the pane title. Windows names the
//! foreground process through `winproc::foreground_name`, so this module is
//! compiled only on macOS and Linux.

/// The executable name behind `pid`, or `None` when it cannot be read.
///
/// A non-positive pid is refused before the syscall. The string is the file
/// name only, never a path.
#[cfg(target_os = "macos")]
pub(super) fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// See the macOS arm.
///
/// `/proc/<pid>/exe` is a link the kernel refuses to resolve for a process
/// owned by someone else, so the `comm` fallback is what keeps a differently
/// owned process from coming back nameless.
#[cfg(target_os = "linux")]
pub(super) fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let name = name.strip_suffix(" (deleted)").unwrap_or(name);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

#[cfg(test)]
mod tests {
    use super::proc_name;

    #[test]
    fn a_non_positive_pid_has_no_name() {
        assert_eq!(proc_name(0), None);
        assert_eq!(proc_name(-1), None);
    }

    #[test]
    fn the_running_process_has_an_executable_name() {
        let name = proc_name(std::process::id() as i32).expect("this process has a name");
        assert!(!name.is_empty());
        assert!(!name.contains('/'));
    }
}
