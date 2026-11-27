//! Shells a pane is started with.
//!
//! Production needs the login shell, the Windows default when a pane names no
//! program, and the default WSL distro a `wsl.exe` pane is filed under. Windows
//! integration tests still ask where Git Bash, Nushell and the installed
//! distros live; those helpers compile only under `cargo test` on Windows.

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::path::PathBuf;

/// The user's login shell, straight from the passwd database.
///
/// `$SHELL` is a snapshot taken when the session logged in, so `chsh` does not
/// move it — a GUI launch inherits whatever was current at login and keeps
/// reporting it until the user logs out. passwd is the live value; `$SHELL` is
/// only the fallback for the rare setup where the lookup fails (a directory
/// service that is down, a uid with no passwd entry).
pub fn login_shell() -> String {
    #[cfg(unix)]
    {
        pick_login_shell(passwd_shell(), std::env::var("SHELL").ok())
    }
    #[cfg(windows)]
    {
        windows_default_shell().to_string()
    }
}

#[cfg(any(unix, test))]
fn pick_login_shell(passwd: Option<String>, env: Option<String>) -> String {
    passwd
        .into_iter()
        .chain(env)
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into())
}

/// `getpwuid_r` — the reentrant form, because `getpwuid` hands back a pointer
/// into a shared static that another thread's lookup can overwrite under us.
#[cfg(unix)]
fn passwd_shell() -> Option<String> {
    let mut buf_len = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n > 0 => n as usize,
        _ => 1024,
    };
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; buf_len];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        // ERANGE just means the buffer was too small; anything else is fatal.
        if rc == libc::ERANGE && buf_len < 64 * 1024 {
            buf_len *= 2;
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_shell.is_null() {
            return None;
        }
        let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) }
            .to_str()
            .ok()?
            .to_string();
        return Some(shell);
    }
}

#[cfg(windows)]
pub fn windows_default_shell() -> &'static str {
    use std::sync::OnceLock;
    static DEFAULT: OnceLock<String> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        find_pwsh7()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "powershell.exe".to_string())
    })
}

#[cfg(windows)]
fn find_pwsh7() -> Option<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramFiles(Arm)"] {
        if let Some(pf) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            let pf = PathBuf::from(pf);
            roots.push(pf.join("PowerShell").join("7"));
            roots.push(pf.join("PowerShell").join("7-preview"));
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        let home = PathBuf::from(home);
        roots.push(home.join(".dotnet").join("tools"));
        roots.push(home.join("scoop").join("shims"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(local).join("Microsoft").join("WindowsApps"));
    }
    pick_first_existing(roots.iter().map(|r| r.join("pwsh.exe")))
        .or_else(|| find_in_path("pwsh.exe"))
}

#[cfg(windows)]
fn pick_first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(windows)]
fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in_path_var(exe, &path)
}

/// Finds an exact executable name using Windows' ordered `PATH` directories.
///
/// The caller supplies the `.exe` suffix explicitly so a directory with the
/// same name is never mistaken for a launchable shell. Keeping the path value
/// separate also makes the lookup testable without mutating process-global
/// environment variables while other tests are running.
#[cfg(windows)]
fn find_in_path_var(exe: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

/// Where `wsl.exe` registers what is installed: one subkey per distro, named by
/// GUID, carrying `DistributionName` and `State`.
#[cfg(windows)]
const LXSS: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";

/// The distro `wsl.exe` launches when no `--distribution` is given, read from
/// the registry (`Lxss\DefaultDistribution` names the per-distro key that
/// carries `DistributionName`). The registry rather than `wsl -l`: this runs
/// on the pane-spawn path, where a microsecond read beats a subprocess.
#[cfg(windows)]
pub fn default_wsl_distro() -> Option<String> {
    let guid = registry_user_string(LXSS, "DefaultDistribution")?;
    let name = registry_user_string(&format!(r"{LXSS}\{guid}"), "DistributionName")?;
    (!name.is_empty()).then_some(name)
}

#[cfg(not(windows))]
pub fn default_wsl_distro() -> Option<String> {
    None
}

#[cfg(windows)]
fn registry_user_string(subkey: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let subkey = wide(subkey);
    let value = wide(value);
    let mut bytes: u32 = 0;
    // SAFETY: a null data pointer makes this a pure sizing call; the key and
    // value names are NUL-terminated UTF-16 owned right above.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != 0 || bytes == 0 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut size = bytes;
    // SAFETY: `buf` is `size` bytes as the sizing call reported;
    // RegGetValueW writes at most that many and NUL-terminates REG_SZ data.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

#[cfg(all(windows, test))]
pub(crate) fn git_bash_path() -> Option<PathBuf> {
    find_git_bash()
}

#[cfg(all(windows, test))]
pub(crate) fn nushell_path() -> Option<PathBuf> {
    find_in_path("nu.exe")
}

#[cfg(all(windows, test))]
pub(crate) fn wsl_distros() -> Vec<String> {
    list_wsl_distros().unwrap_or_default()
}

#[cfg(all(windows, test))]
fn find_git_bash() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            candidates.push(PathBuf::from(pf).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    pick_first_existing(candidates)
}

/// Distros that exist to carry a container runtime, not to be typed into.
#[cfg(test)]
const NOT_FOR_TYPING: [&str; 2] = ["docker-desktop", "rancher-desktop"];

#[cfg(test)]
fn worth_offering(name: &str) -> bool {
    !name.is_empty() && !NOT_FOR_TYPING.iter().any(|hidden| name.starts_with(hidden))
}

/// Installed distros from the `Lxss` registry. `None` means the walk
/// could not be trusted, and the caller falls back to `wsl.exe`.
#[cfg(all(windows, test))]
fn registered_wsl_distros() -> Option<Vec<String>> {
    let guids = registry_user_subkeys(LXSS)?;
    let names: Vec<String> = guids
        .iter()
        .map(|guid| format!(r"{LXSS}\{guid}"))
        // A key with no `State` at all is taken at its word: the absent value
        // is not evidence of a broken install, and inventing one would be how
        // this hides a working distro.
        .filter(|key| registry_user_dword(key, "State").unwrap_or(INSTALLED) == INSTALLED)
        .filter_map(|key| registry_user_string(&key, "DistributionName"))
        .filter(|name| worth_offering(name))
        .collect();

    // Subkeys but nothing to show for them is not an answer either: every name
    // unreadable has the shape of a permissions problem, not of a machine with
    // no distros on it — that machine has an empty `Lxss`, and says so.
    if names.is_empty() && !guids.is_empty() {
        return None;
    }
    Some(names)
}

/// `State` of a distro that finished installing and has not started leaving.
#[cfg(all(windows, test))]
const INSTALLED: u32 = 1;

#[cfg(all(windows, test))]
fn registry_user_dword(subkey: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let (subkey, value) = (wide(subkey), wide(value));
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated and owned here, and `data` is a
    // live u32 exactly `size` bytes long, which is what a DWORD read writes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (rc == 0).then_some(data)
}

/// The names of a key's subkeys, or `None` if they could not all be read.
///
/// All or nothing on purpose. A caller that cannot tell a short list from a
/// complete one would quietly drop distros: the walk is by index, so a key
/// that changes underneath it —
/// `wsl --unregister` running right now, a Store install rewriting `Lxss` —
/// ends early, and reporting that as the answer is worse than admitting it.
#[cfg(all(windows, test))]
fn registry_user_subkeys(subkey: &str) -> Option<Vec<String>> {
    use windows_sys::Win32::Foundation::ERROR_NO_MORE_ITEMS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };

    let subkey: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated and owned here; `key` is written only
    // on success and closed on every path out below.
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return None;
    }

    let mut names = Vec::new();
    // A registry key name is at most 255 characters, plus the terminator.
    let mut buf = [0u16; 256];
    let mut ended_with = None;
    for index in 0.. {
        let mut len = buf.len() as u32;
        // SAFETY: `buf` really is `len` units long, and every pointer that is
        // not wanted is null, which this call documents as "do not report it".
        let rc = unsafe {
            RegEnumKeyExW(
                key,
                index,
                buf.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            ended_with = Some(rc);
            break;
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
    }

    // SAFETY: `key` was opened above and is not used after this.
    unsafe { RegCloseKey(key) };
    // There is one honest way for the walk to end. Anything else — the key
    // deleted underneath it, a name that would not fit — leaves a list that is
    // short by an unknown amount, which nobody downstream can tell from a real
    // one, so say nothing instead.
    (ended_with == Some(ERROR_NO_MORE_ITEMS)).then_some(names)
}

#[cfg(all(windows, test))]
fn list_wsl_distros() -> Option<Vec<String>> {
    if let Some(registered) = registered_wsl_distros() {
        return Some(registered);
    }

    // The registry would not answer — no `Lxss` key at all, or a walk of it that
    // ended somewhere other than the end. Either way this is not a "there are no
    // distros" to pass on, so ask the slow way rather than claim there is nothing.
    log::debug!("{LXSS} gave no usable answer; falling back to `wsl -l -q`");
    let mut cmd = std::process::Command::new("wsl.exe");
    cmd.args(["-l", "-q"]);
    let output = match crate::core::proc::hide_console(&mut cmd).output() {
        Ok(output) => output,
        Err(e) => {
            log::warn!("could not list the WSL distros: {e}");
            return None;
        }
    };
    if !output.status.success() {
        return None;
    }
    Some(parse_wsl_list(&output.stdout))
}

#[cfg(test)]
fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let text = String::from_utf16_lossy(&units);
    text.lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|l| worth_offering(l))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// passwd is the live value; `$SHELL` is a login-time snapshot that `chsh`
    /// cannot move, so it must never win.
    #[test]
    fn login_shell_prefers_passwd_over_a_stale_env() {
        assert_eq!(
            pick_login_shell(
                Some("/opt/homebrew/bin/bash".into()),
                Some("/bin/zsh".into())
            ),
            "/opt/homebrew/bin/bash"
        );
        // passwd unreadable, or the entry is blank — fall back to $SHELL
        assert_eq!(pick_login_shell(None, Some("/bin/zsh".into())), "/bin/zsh");
        assert_eq!(
            pick_login_shell(Some("  ".into()), Some("/bin/zsh".into())),
            "/bin/zsh"
        );
        // neither available
        assert_eq!(pick_login_shell(None, None), "sh");
        assert_eq!(
            pick_login_shell(Some(String::new()), Some(String::new())),
            "sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn passwd_shell_reads_this_users_entry() {
        // Every account running the suite has a shell in passwd; the point is
        // that the lookup works and returns an absolute path, not a specific one.
        let got = passwd_shell().expect("passwd lookup should succeed");
        assert!(
            got.starts_with('/'),
            "expected an absolute path, got {got:?}"
        );
    }

    #[test]
    fn parse_wsl_list_decodes_utf16le_and_filters() {
        let text = "Ubuntu\r\ndocker-desktop\r\ndocker-desktop-data\r\nDebian\r\n\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_wsl_list(&bytes), vec!["Ubuntu", "Debian"]);
    }

    /// A directory named `pwsh.exe` is not a shell. The first real file later
    /// on `PATH` is.
    #[cfg(windows)]
    #[test]
    fn windows_path_probe_finds_the_first_pwsh_file() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let directory_named_like_pwsh = first.path().join("pwsh.exe");
        std::fs::create_dir(&directory_named_like_pwsh).unwrap();
        let expected = second.path().join("pwsh.exe");
        std::fs::write(&expected, b"test executable placeholder").unwrap();

        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(find_in_path_var("pwsh.exe", &path), Some(expected));
    }
}

/// The registry default has to be a distro the same walk would list.
#[cfg(all(windows, test))]
mod wsl_tests {
    #[test]
    fn the_default_distro_is_one_of_the_installed_ones() {
        let installed = super::wsl_distros();
        if installed.is_empty() {
            eprintln!("skipping: no WSL distributions installed");
            return;
        }
        let default = super::default_wsl_distro()
            .expect("a machine with installed distros names a default in Lxss");
        assert!(
            installed.contains(&default),
            "registry default {default:?} not in {installed:?}"
        );
    }
}
