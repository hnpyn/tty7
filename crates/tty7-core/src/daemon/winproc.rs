use std::collections::{HashSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub parent: u32,
}

fn walk(procs: &[Proc], root: u32) -> Vec<(u32, u32)> {
    let mut seen = HashSet::new();
    seen.insert(root);
    let mut frontier = VecDeque::new();
    frontier.push_back((root, 0u32));
    let mut out = Vec::new();
    while let Some((parent, depth)) = frontier.pop_front() {
        for p in procs {
            if p.parent == parent && p.pid != parent && seen.insert(p.pid) {
                out.push((depth + 1, p.pid));
                frontier.push_back((p.pid, depth + 1));
            }
        }
    }
    out
}

pub(crate) fn descendants(procs: &[Proc], root: u32) -> Vec<u32> {
    let mut walked = walk(procs, root);
    walked.sort_by_key(|&(depth, _)| std::cmp::Reverse(depth));
    walked.into_iter().map(|(_, pid)| pid).collect()
}

pub(crate) fn snapshot() -> Vec<Proc> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                out.push(Proc {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                });
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

pub(crate) fn terminate(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Makes Ctrl+C reach the processes this one goes on to spawn.
///
/// Windows keeps an "ignore Ctrl+C" bit per process, and a child inherits it at
/// creation. Anything that sets it on the daemon — being created with
/// `CREATE_NEW_PROCESS_GROUP`, or simply being launched from a shell that
/// already had it — silently disarms Ctrl+C in every pane, because the pane's
/// shell and everything it runs are all descendants. The pane still writes 0x03
/// into the pty and conhost still turns it into a keypress; what never happens
/// is the CTRL_C_EVENT, so `go run` and `npm install` keep going (#451, #314).
///
/// A `NULL` routine with `FALSE` clears the bit, and it is what descendants
/// created afterwards inherit. Cheap and idempotent, so the pane spawn path
/// calls it rather than relying on a single well-placed call at startup.
pub(crate) fn allow_ctrl_c_in_children() {
    // SAFETY: A NULL handler routine is the documented spelling of "stop
    // ignoring Ctrl+C"; there is no handler list entry to keep alive.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use std::io::{Read as _, Write as _};
    use std::os::windows::process::CommandExt as _;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    const RESULT_ENV: &str = "TTY7_CTRL_C_INHERITED_IGNORE_RESULT";
    const TEST_NAME: &str =
        "daemon::winproc::ctrl_c_tests::a_pane_survives_an_inherited_ignore_ctrl_c";

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The reported bug (#451, #314): nothing in a tty7 pane could be
    /// interrupted, so `go run` and `npm install` ran on after Ctrl+C.
    ///
    /// The keystroke path was never at fault — the pane does write 0x03 into the
    /// pty, and conhost does turn it into a keypress. What never happened was
    /// the CTRL_C_EVENT, because the daemon carried Windows' "ignore Ctrl+C"
    /// process state and every ConPTY child inherits it.
    ///
    /// So the state has to be *inherited* here rather than switched on in
    /// place — that is the shape the daemon was in, and it is the only version a
    /// single `SetConsoleCtrlHandler(NULL, FALSE)` has to be able to undo. An
    /// intermediate process created exactly as the daemon used to be supplies
    /// it, and then runs both arms: a pane spawned without the fix must hang,
    /// and a pane spawned after it must not. Testing only the second arm would
    /// pass on its own on a machine whose runner never had the bit set.
    #[test]
    fn a_pane_survives_an_inherited_ignore_ctrl_c() {
        if let Some(result) = std::env::var_os(RESULT_ENV) {
            let unfixed = interruptible();
            super::allow_ctrl_c_in_children();
            let fixed = interruptible();
            std::fs::write(result, format!("{unfixed} {fixed}")).expect("write the arms' verdict");
            return;
        }

        let result =
            std::env::temp_dir().join(format!("tty7-pane-ctrl-c-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&result);
        let status = Command::new(std::env::current_exe().expect("locate the test executable"))
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(RESULT_ENV, &result)
            // Exactly how the daemon used to be created. CREATE_NEW_PROCESS_GROUP
            // is what leaves the inherited ignore behind.
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run the arms in a process that inherits the ignore");
        assert!(status.success(), "the inner test process must finish");

        let verdict = std::fs::read_to_string(&result).unwrap_or_else(|_| "MISSING".into());
        let _ = std::fs::remove_file(&result);
        assert_eq!(
            verdict.trim(),
            "false true",
            "a pane spawned under an inherited ignore must be interruptible only after \
             allow_ctrl_c_in_children (got `{}`); `true true` means the ignore never took \
             hold and the test proves nothing, `false false` is the bug in #451 and #314",
            verdict.trim()
        );
    }

    /// Runs a shell in a pane the way `DaemonPane::spawn` does, interrupts a
    /// command that would otherwise never end, and reports whether the shell
    /// came back.
    ///
    /// The observable is the shell, not the interrupted command: after the ^C,
    /// `cmd` gets its prompt back and acts on the `exit` typed behind it. If the
    /// ^C is swallowed, `ping` still owns the console, the `exit` is never read,
    /// and the shell outlives the deadline. Nothing here reads a message, so it
    /// holds on a non-English Windows too.
    fn interruptible() -> bool {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty the way a pane does");
        let mut child = pty
            .slave
            .spawn_command(CommandBuilder::new("cmd.exe"))
            .expect("spawn the shell");
        drop(pty.slave);

        // ConPTY stalls its client once the output pipe fills, so this has to
        // keep running whatever else happens. The byte count it keeps is how the
        // test paces itself instead of guessing at sleeps.
        let mut reader = pty.master.try_clone_reader().expect("clone the reader");
        let written = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&written);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                counter.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let bytes = || written.load(std::sync::atomic::Ordering::Relaxed);
        // Waits for `rounds` separate batches of output. Two of them means the
        // running command is producing its own lines on its own schedule, which
        // is the only reliable sign that it — not the shell — holds the console.
        // Interrupting before that reaches nobody, and would look like this bug
        // on a slow machine for a reason that has nothing to do with the fix.
        let settle = |rounds: usize| {
            let give_up = Instant::now() + Duration::from_secs(20);
            for _ in 0..rounds {
                let before = bytes();
                while bytes() == before && Instant::now() < give_up {
                    std::thread::sleep(Duration::from_millis(50));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        };

        let mut writer = pty.master.take_writer().expect("take the writer");
        let mut type_in = |keys: &[u8]| {
            writer.write_all(keys).expect("write to the pty");
            writer.flush().expect("flush the pty");
        };
        settle(1); // the shell's banner and first prompt
        // Runs until interrupted, and holds the console while it does.
        type_in(b"ping -t 127.0.0.1\r");
        settle(3); // the echoed line, then replies on ping's own clock
        type_in(&[0x03]);
        std::thread::sleep(Duration::from_millis(500));
        type_in(b"exit\r");

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match child.try_wait().expect("poll the shell") {
                Some(_) => return true,
                None if Instant::now() >= deadline => {
                    // Killing the shell leaves the `ping` it never interrupted
                    // running forever, so take the subtree with it. Read the
                    // subtree while the shell is still alive: once it is gone
                    // its pid can be handed to somebody else, and a snapshot
                    // taken after that names a stranger's children.
                    let doomed = child
                        .process_id()
                        .map(|shell| super::descendants(&super::snapshot(), shell))
                        .unwrap_or_default();
                    let _ = child.kill();
                    let _ = child.wait();
                    for pid in doomed {
                        super::terminate(pid);
                    }
                    return false;
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32) -> Proc {
        Proc { pid, parent }
    }

    #[test]
    fn descendants_collects_only_the_shell_subtree() {
        let procs = vec![
            p(1, 0),
            p(100, 1),
            p(200, 100),
            p(300, 200),
            p(201, 100),
            p(999, 1),
        ];
        let mut got = descendants(&procs, 100);
        got.sort();
        assert_eq!(got, vec![200, 201, 300]);
    }

    #[test]
    fn descendants_are_ordered_deepest_first() {
        let procs = vec![p(100, 1), p(200, 100), p(300, 200)];
        assert_eq!(descendants(&procs, 100), vec![300, 200]);
    }

    #[test]
    fn descendants_survive_a_pid_reuse_cycle() {
        let procs = vec![p(100, 200), p(200, 100)];
        assert_eq!(descendants(&procs, 100), vec![200]);
    }

    #[test]
    fn descendants_survive_self_parenting() {
        let procs = vec![p(100, 1), p(100, 100)];
        assert!(descendants(&procs, 100).is_empty());
    }

    #[test]
    fn descendants_empty_without_children() {
        let procs = vec![p(100, 1), p(999, 1)];
        assert!(descendants(&procs, 100).is_empty());
    }
}
