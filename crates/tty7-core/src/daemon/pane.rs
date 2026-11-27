use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::core::clipboard::{
    ClipboardSniffer, Segment as ClipboardSegment, Sniffed as ClipboardSniffed,
};
use crate::core::kitty_graphics::{GraphicsSniffer, Segment, Sniffed};
use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{
    DaemonMsg, MAX_FRAME, RemoteContext, RemoteKind, ShellSpec, WinSize,
};
use crate::daemon::shell_integration;

#[cfg(windows)]
fn default_prog() -> CommandBuilder {
    CommandBuilder::new(crate::core::shells::windows_default_shell())
}

#[cfg(not(windows))]
fn default_prog() -> CommandBuilder {
    default_prog_with_override(detected_shell_override())
}

#[cfg(not(windows))]
fn default_prog_with_override(shell_override: Option<String>) -> CommandBuilder {
    let cmd = CommandBuilder::new_default_prog();
    let portable_shell = cmd.get_shell();
    if let Some(shell) = shell_override.filter(|shell| shell != &portable_shell) {
        return CommandBuilder::new(&shell);
    }
    cmd
}

#[cfg(not(windows))]
fn detected_shell_override() -> Option<String> {
    let path = std::env::var_os(crate::daemon::DETECTED_SHELL_ENV)?;
    usable_shell_path(path)
}

#[cfg(not(windows))]
fn usable_shell_path(path: std::ffi::OsString) -> Option<String> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() || !path.is_file() {
        return None;
    }
    path.into_os_string().into_string().ok()
}

#[cfg(windows)]
fn default_shell_name(_cmd: &CommandBuilder) -> String {
    crate::core::shells::windows_default_shell().to_string()
}

/// The shell `cmd` will actually run. `get_shell` names the login shell
/// whatever `cmd` holds, and the detected-shell override swaps the program
/// for another one: tty7 launched from bash with zsh as the login shell ran
/// bash with zsh's integration, which is none at all.
#[cfg(not(windows))]
fn default_shell_name(cmd: &CommandBuilder) -> String {
    match cmd.get_argv().first() {
        Some(program) if !cmd.is_default_prog() => program.to_string_lossy().into_owned(),
        _ => cmd.get_shell(),
    }
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
struct ChosenShell {
    program: String,
    args: Vec<String>,
    args_are_tty7_defaults: bool,
}

fn has_custom_args(chosen: Option<&ChosenShell>) -> bool {
    chosen.is_some_and(|c| !c.args.is_empty() && !c.args_are_tty7_defaults)
}

fn choose_shell(
    spawn_override: Option<ShellSpec>,
    configured: Option<(String, Vec<String>)>,
) -> Option<ChosenShell> {
    spawn_override
        .map(|s| ChosenShell {
            program: s.program,
            args: s.args,
            args_are_tty7_defaults: s.args_are_tty7_defaults,
        })
        .or_else(|| {
            configured.map(|(program, args)| ChosenShell {
                program,
                args,
                args_are_tty7_defaults: false,
            })
        })
}

fn apply_shell_integration(
    cmd: &mut CommandBuilder,
    resolved_program: &str,
    integration: &shell_integration::Injection,
) {
    if integration.replaces_argv || (cmd.is_default_prog() && !integration.args.is_empty()) {
        *cmd = CommandBuilder::new(resolved_program);
    }
    cmd.args(&integration.args);
    for (k, v) in &integration.env {
        cmd.env(k, v);
    }
}

struct SpawnConfig {
    cmd: CommandBuilder,
    initial_cwd: Option<PathBuf>,
    integration_dir: Option<PathBuf>,
    remote: Option<RemoteContext>,
}

/// Why a configured shell cannot be run, in one sentence, before anything
/// tries. Without it a missing program arrives at the window wrapped four
/// deep — "daemon refused Spawn: spawn failed: Unable to spawn … (ENOENT: No
/// such file or directory)" — and the one fact that matters is buried in it.
///
/// Only a program given as a path can be checked here; a bare name is resolved
/// through PATH by the OS, and guessing at that would be worse than silence.
fn shell_program_problem(program: &str) -> Option<String> {
    let path = std::path::Path::new(program);
    if path.components().count() < 2 {
        return None;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Some(format!("no such shell on this machine: {program}"));
    };
    if meta.is_dir() {
        return Some(format!("the configured shell is a directory: {program}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Some(format!("the configured shell is not executable: {program}"));
        }
    }
    None
}

fn build_spawn_config(
    pane: u64,
    cwd: Option<PathBuf>,
    shell: Option<ShellSpec>,
    use_config_shell: bool,
) -> anyhow::Result<SpawnConfig> {
    let initial_cwd = initial_working_directory(cwd);
    let from_config = if use_config_shell {
        crate::core::config::shell_command()
    } else {
        None
    };
    let configured = choose_shell(shell, from_config);
    if let Some(problem) = configured
        .as_ref()
        .and_then(|c| shell_program_problem(&c.program))
    {
        anyhow::bail!(problem);
    }
    let remote = wsl_remote_context(configured.as_ref());
    let (cmd, integration_dir) = build_shell_command(configured, &initial_cwd, pane)?;
    Ok(SpawnConfig {
        cmd,
        initial_cwd,
        integration_dir,
        remote,
    })
}

fn wsl_remote_context(shell: Option<&ChosenShell>) -> Option<RemoteContext> {
    if !cfg!(windows) {
        return None;
    }
    let chosen = shell?;
    let base = std::path::Path::new(&chosen.program)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    if base.strip_suffix(".exe").unwrap_or(&base) != "wsl" {
        return None;
    }
    Some(RemoteContext {
        kind: RemoteKind::Wsl,
        argv: Vec::new(),
        // No `--distribution` means wsl.exe launches the default distro —
        // name it here, so every consumer of the context (the `\\wsl$`
        // completion route, paste-path rewriting, host labels) gets a real
        // distro instead of an empty placeholder.
        target: shell_integration::wsl_distro(&chosen.args)
            .or_else(crate::core::shells::default_wsl_distro)
            .unwrap_or_default(),
    })
}

fn build_shell_command(
    configured: Option<ChosenShell>,
    initial_cwd: &Option<PathBuf>,
    pane: u64,
) -> anyhow::Result<(CommandBuilder, Option<PathBuf>)> {
    let mut cmd = match &configured {
        Some(chosen) => {
            let mut c = CommandBuilder::new(&chosen.program);
            c.args(&chosen.args);
            c
        }
        None => default_prog(),
    };
    let resolved_program = match &configured {
        Some(chosen) => chosen.program.clone(),
        None => default_shell_name(&cmd),
    };

    let integration = shell_integration::setup(
        Some(&resolved_program),
        configured.as_ref().map_or(&[][..], |c| c.args.as_slice()),
        has_custom_args(configured.as_ref()),
    );
    if let Some(integration) = &integration {
        apply_shell_integration(&mut cmd, &resolved_program, integration);
    }
    let integration_dir = integration.as_ref().and_then(|i| i.dir.clone());
    let launched = launched_shell_program(&cmd, &resolved_program);
    apply_common_command_setup(&mut cmd, initial_cwd, pane, &launched);
    Ok((cmd, integration_dir))
}

/// The program the pane is really about to exec.
///
/// `resolved_program` is what tty7 *decided* to run before shell integration
/// had its say, and for a default-prog builder that decision is
/// `CommandBuilder::get_shell()` — the passwd entry — even when
/// `default_prog()` swapped the builder for the shell that launched the daemon.
/// argv survives every rewrite on the way here, including the one an
/// argv-replacing injection performs, so it is the honest answer. An empty argv
/// means the builder is still the untouched default prog, which is precisely
/// the case `resolved_program` already describes.
fn launched_shell_program(cmd: &CommandBuilder, resolved_program: &str) -> String {
    cmd.get_argv()
        .first()
        .map(|argv0| argv0.to_string_lossy().into_owned())
        .unwrap_or_else(|| resolved_program.to_string())
}

fn initial_working_directory(cwd: Option<PathBuf>) -> Option<PathBuf> {
    let fallback = std::env::current_dir()
        .ok()
        .filter(|d| d != std::path::Path::new("/"))
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
    let forced = crate::core::config::working_directory_base();
    // A configured "Start in" path that is not a directory can never win the
    // pick below, so every new pane silently lands on the fallback and the
    // mistyped path reads as a tty7 bug (#601). Settings refuses to save one
    // now, but a hand-edited config.json can still hold one — name it, with
    // the reason, at the moment it actually costs something (an explicit cwd
    // that resolves never consults the config, so that case stays quiet).
    if let Some(dir) = &forced
        && !dir.is_dir()
        && cwd.as_ref().is_none_or(|d| !d.is_dir())
    {
        log::warn!(
            "configured working directory {} is not a directory; new panes fall back",
            dir.display()
        );
    }
    [cwd, forced, fallback]
        .into_iter()
        .flatten()
        .find(|d| d.is_dir())
}

#[cfg(any(target_os = "macos", test))]
fn locale_fallback_is_needed(
    extra_env: &std::collections::HashMap<String, String>,
    mut inherited: impl FnMut(&str) -> Option<String>,
) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"].iter().all(|key| {
        !extra_env.contains_key(*key) && inherited(key).as_deref().is_none_or(str::is_empty)
    })
}

#[cfg(target_os = "macos")]
const LOCALE_DEFINITION_DIR: &str = "/usr/share/locale";

#[cfg(any(target_os = "macos", test))]
const FALLBACK_CHARACTER_LOCALES: [&str; 2] = ["C.UTF-8", "en_US.UTF-8"];

/// The variable the locale fallback sets.
///
/// It has to be one a shell consults for *every* category. `LC_CTYPE` alone
/// fixes character handling and leaves collation, time and numbers at `C`; a
/// shell that then asks `setlocale(LC_COLLATE, "")` finds no variable to read
/// and bash warns `setlocale: LC_COLLATE: cannot change locale ()` once per
/// category. (zsh and fish swallow the failure, so they look fine while being
/// just as half-configured.) `LC_ALL` would also cover everything, but it wins
/// over every `LC_*` the user's own rc files set afterwards — `LANG` loses to
/// them, which is what a fallback should do.
#[cfg(any(target_os = "macos", test))]
const LOCALE_FALLBACK_KEY: &str = "LANG";

#[cfg(any(target_os = "macos", test))]
fn character_locale(identifier: Option<&str>, exists: impl Fn(&str) -> bool) -> Option<String> {
    identifier
        .and_then(posix_locale_stem)
        .map(|stem| format!("{stem}.UTF-8"))
        .into_iter()
        .chain(FALLBACK_CHARACTER_LOCALES.iter().map(|s| (*s).to_string()))
        .find(|candidate| exists(candidate))
}

#[cfg(any(target_os = "macos", test))]
fn posix_locale_stem(identifier: &str) -> Option<String> {
    let base = identifier.split('@').next()?;
    let mut parts = base.split(['_', '-']).filter(|s| !s.is_empty());
    let language = parts
        .next()
        .filter(|l| (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic()))?;
    let region = parts.next_back().filter(|r| {
        (r.len() == 2 && r.chars().all(|c| c.is_ascii_alphabetic()))
            || (r.len() == 3 && r.chars().all(|c| c.is_ascii_digit()))
    })?;
    Some(format!(
        "{}_{}",
        language.to_ascii_lowercase(),
        region.to_ascii_uppercase()
    ))
}

#[cfg(target_os = "macos")]
fn system_locale_identifier() -> Option<String> {
    use std::os::raw::c_void;

    type CFLocaleRef = *const c_void;
    type CFStringRef = *const c_void;

    /// `kCFStringEncodingUTF8`.
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFLocaleCopyCurrent() -> CFLocaleRef;
        fn CFLocaleGetIdentifier(locale: CFLocaleRef) -> CFStringRef;
        fn CFStringGetCString(
            the_string: CFStringRef,
            buffer: *mut std::ffi::c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    unsafe {
        let locale = CFLocaleCopyCurrent();
        if locale.is_null() {
            return None;
        }
        let identifier = CFLocaleGetIdentifier(locale);
        let out = if identifier.is_null() {
            None
        } else {
            // Locale identifiers (`en_US`, `zh_Hans_CN`) are far shorter than this.
            let mut buf = [0u8; 256];
            let ok = CFStringGetCString(
                identifier,
                buf.as_mut_ptr().cast(),
                buf.len() as isize,
                CF_STRING_ENCODING_UTF8,
            );
            if ok == 0 {
                None
            } else {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8(buf[..len].to_vec()).ok()
            }
        };
        CFRelease(locale);
        out
    }
}

const TERM_PROGRAM_NAME: &str = "tty7";
const NTTY7_ENV_MARKER: &str = "NTTY7";

/// The config dir this server runs on, not a socket path.
///
/// A client needs *two* endpoints — control and pane — and they are derived
/// from the config dir by rules that already live in this crate
/// (`control_socket_path`, `transport::socket_path_for`), including a hashed
/// fallback when the directory is too long for `sun_path`. Publishing one
/// socket path and letting the client reconstruct the other from it means a
/// second, independent derivation that can and did disagree with the first.
/// Publishing the directory instead means a CLI in this shell resolves both
/// endpoints with the very same functions the server used to open them.
const NTTY7_CONFIG_DIR_ENV: &str = "NTTY7_CONFIG_DIR";
pub(crate) const NTTY7_PANE_ENV: &str = "NTTY7_PANE";

fn config_dir_env() -> Option<String> {
    // The shell integration and a nested `ntty7` both read this. It is the
    // real config directory (`--config-dir`, else `~/.config/ntty7`), never a
    // per-process temp dir.
    crate::core::config::config_dir_path().map(|p| p.display().to_string())
}

/// The shell the pane is running, not the one the passwd entry names.
///
/// Everything that spawns "the user's shell" — tmux's `default-shell`, `sudo
/// -s`, an editor's `!` escape, a coding agent picking a quoting dialect —
/// reads `$SHELL`. A GUI launch inherits the login session's snapshot of it,
/// so a pane told to run fish still advertised `/bin/zsh` and every one of
/// those consumers walked off into the wrong shell (#342). Inside a pane, the
/// shell tty7 was told to run *is* the user's shell, so that is what we name.
///
/// Note this is the opposite direction from `shells::login_shell()`, which
/// deliberately ignores `$SHELL` in favour of passwd: that answers "which
/// shell did the user log in with", which is still the login shell. This
/// answers "which shell is this pane", which is not the same question.
const SHELL_ENV: &str = "SHELL";

/// The absolute path to name in `$SHELL`, or `None` when there is none to
/// promise.
///
/// `$SHELL` is a POSIX contract, so the path rules here are POSIX ones rather
/// than the host's — this only ever describes a pane on a unix daemon (see the
/// caller), and hard-coding them keeps the resolution testable on any host.
///
/// A configured command may be bare (`fish`), because a bare command is what
/// the shell inventory keeps when it wants PATH to decide which install wins.
/// Consumers of `$SHELL` exec it directly, and many of them do so with a PATH
/// of their own, so a bare name there is not the same binary the pane runs.
/// Resolve it the way exec would, against the PATH the pane will inherit, and
/// leave `$SHELL` untouched when that finds nothing — a stale but honest login
/// shell beats a name that resolves somewhere else.
fn shell_env_path(
    program: &str,
    path_var: impl FnOnce() -> Option<String>,
    is_file: impl Fn(&str) -> bool,
) -> Option<String> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    if program.starts_with('/') {
        return Some(program.to_string());
    }
    // Anything else with a separator is relative to a working directory, and
    // the reader's is not necessarily the pane's. Only a bare name is worth
    // resolving.
    if program.contains('/') {
        return None;
    }
    path_var()?
        .split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| format!("{}/{program}", dir.trim_end_matches('/')))
        .find(|candidate| is_file(candidate))
}

const CAPABILITY_ENV: [&str; 2] = ["TERM", "COLORTERM"];

fn names_capability_env(key: &str) -> bool {
    CAPABILITY_ENV.iter().any(|cap| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case(cap)
        } else {
            key == *cap
        }
    })
}

fn pane_environment(
    extra_env: &std::collections::HashMap<String, String>,
    // Only Windows has a use for it — see the `COLORFGBG` block below.
    #[cfg_attr(not(windows), allow(unused_variables))] dark: bool,
    pane: u64,
    shell: &str,
) -> Vec<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    let mut env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        (NTTY7_ENV_MARKER.to_string(), version.to_string()),
        ("TERM_PROGRAM".to_string(), TERM_PROGRAM_NAME.to_string()),
        ("TERM_PROGRAM_VERSION".to_string(), version.to_string()),
        // tty7 renders OSC 8 links, but `supports-hyperlinks` (Claude Code,
        // most Node CLIs) only trusts a fixed TERM_PROGRAM list and strips
        // them otherwise; this is the override it honours. It is checked
        // before the isTTY test, so such a tool now writes OSC 8 into
        // `> file` and `| less` too. `FORCE_HYPERLINK=0` in the `env` config
        // or the shell's rc turns it back off.
        ("FORCE_HYPERLINK".to_string(), "1".to_string()),
        (NTTY7_PANE_ENV.to_string(), pane.to_string()),
    ];
    #[cfg(windows)]
    if !extra_env
        .keys()
        .any(|key| key.eq_ignore_ascii_case("COLORFGBG"))
    {
        // The bundled ConPTY forwards OSC 11 queries, so this is no longer the
        // only answer a Windows pane gets — but the in-box conhost still
        // swallows them, and that is what a build without `conpty.dll` beside
        // it runs on. Keep the conventional fallback hint for that case, and
        // for applications that never learned to ask. An explicit user override
        // still wins, below.
        let colorfgbg = if dark { "15;0" } else { "0;15" };
        env.push(("COLORFGBG".to_string(), colorfgbg.to_string()));
    }
    if let Some(dir) = config_dir_env() {
        env.push((NTTY7_CONFIG_DIR_ENV.to_string(), dir));
    }
    // Names the pane's own history file; the integration snippet is what
    // decides to use it, once the user's rc has said where their history was.
    if let Some((key, value)) = crate::daemon::history::env_for(pane, shell) {
        env.push((key, value));
    }
    // Windows is deliberately left out. `SHELL` is not a native concept there —
    // neither cmd nor PowerShell reads it — and the tools that do read it are
    // the POSIX emulations: MSYS/Git Bash, Cygwin, and WSL, which take `WSLENV`
    // exports as Linux paths. All of them want a POSIX path, and every shell
    // this branch could name is a Windows one, so setting it would point them
    // at something they cannot exec. That also covers the WSL pane, whose
    // program is `wsl.exe` rather than a shell at all: the distro's own login
    // shell is the right answer inside it, and leaving `SHELL` alone is how
    // that answer survives.
    //
    // Resolution runs against the PATH the pane will actually inherit, so a
    // user who redirects PATH in their `env` block gets the binary that PATH
    // resolves rather than the daemon's.
    if !cfg!(windows)
        && let Some(path) = shell_env_path(
            shell,
            || {
                extra_env
                    .get("PATH")
                    .cloned()
                    .or_else(|| std::env::var("PATH").ok())
            },
            |candidate| std::path::Path::new(candidate).is_file(),
        )
    {
        env.push((SHELL_ENV.to_string(), path));
    }
    env.extend(
        extra_env
            .iter()
            .filter(|(k, _)| !names_capability_env(k))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env
}

/// zsh integration works by pointing `ZDOTDIR` at tty7's redirector dir, which
/// then sources the user's own startup files from `NTTY7_USER_ZDOTDIR`. A
/// `ZDOTDIR` in the configured `env` block is applied after that and used to
/// win outright: zsh read the user's files directly and the pane came up with
/// no prompt marks, no editor, no suggestions. When the redirector is in
/// place, the configured value is where the user's files live, so it goes
/// where the redirector looks for them instead.
fn reroute_configured_zdotdir(
    env: Vec<(String, String)>,
    zsh_integration: bool,
) -> Vec<(String, String)> {
    if !zsh_integration {
        return env;
    }
    env.into_iter()
        .map(|(k, v)| {
            if k == "ZDOTDIR" {
                ("NTTY7_USER_ZDOTDIR".to_string(), v)
            } else {
                (k, v)
            }
        })
        .collect()
}

fn apply_common_command_setup(
    cmd: &mut CommandBuilder,
    initial_cwd: &Option<PathBuf>,
    pane: u64,
    shell: &str,
) {
    if let Some(dir) = initial_cwd {
        cmd.cwd(dir);
        // A new pane inherits the directory the last one *reported*, which is
        // the logical path its shell was showing — `/tmp/x`, not the
        // `/private/tmp/x` the kernel resolves it to. Handing that to `cwd()`
        // alone loses the distinction: the child starts in the resolved
        // directory and its shell falls back to `getcwd()`, so one tab read
        // `/tmp/x` while every tab opened from it read `/private/tmp/x` — the
        // same directory under two names, side by side in the sidebar.
        //
        // `PWD` is how a shell is told which of a directory's names it arrived
        // by; it is what `cd` sets, and what Terminal.app and iTerm2 pass for
        // this reason. POSIX has the shell verify it — a `PWD` that does not
        // name the directory the process is really in is discarded, confirmed
        // here against zsh, bash, sh and fish — so this can correct the name
        // and cannot invent one.
        //
        // Unix only: a Windows shell that reads `PWD` at all wants it in its
        // own idiom (`/c/x` under Git Bash), so a native path would just be
        // discarded by the same check.
        #[cfg(unix)]
        cmd.env("PWD", dir);
    }
    // The AppImage runtime exports `ARGV0` (the path the image was launched
    // by) into tty7's own environment, and the pane would inherit it. zsh
    // treats an exported `ARGV0` as the argv[0] of every external command it
    // runs, so each program in the pane is told it is `tty7-….AppImage` — and
    // multiplexers like Volta's shims, which pick the tool from argv[0], fail
    // with "Could not find executable" (#1100). Nothing outside a single
    // command line ever wants it set, so the pane starts without it; the
    // configured `env` block, applied below, can still put it back.
    cmd.env_remove("ARGV0");

    let extra_env = crate::core::config::extra_env();

    // Windows hands every process a private copy of the environment at spawn
    // time and never updates it, so a daemon that has been running since before
    // an installer touched `HKCU\Environment` would give a brand-new pane its
    // startup `PATH` (#333). Re-read both hives here and pin the merge onto the
    // command; the pane-specific variables below (and the configured overrides
    // they carry) are applied afterwards and still win.
    #[cfg(windows)]
    for (k, v) in crate::daemon::windows_env::refreshed_pane_environment(&extra_env) {
        cmd.env(k, v);
    }

    let dark = crate::core::appearance::appearance().dark;
    let injected_zdotdir = cmd
        .get_env("ZDOTDIR")
        .and_then(|z| z.to_str())
        .filter(|z| shell_integration::is_our_zdotdir(z))
        .map(str::to_owned);
    for (k, v) in reroute_configured_zdotdir(
        pane_environment(&extra_env, dark, pane, shell),
        injected_zdotdir.is_some(),
    ) {
        cmd.env(k, v);
    }

    #[cfg(target_os = "macos")]
    if locale_fallback_is_needed(&extra_env, |key| std::env::var(key).ok())
        && let Some(locale) = character_locale(system_locale_identifier().as_deref(), |name| {
            std::path::Path::new(LOCALE_DEFINITION_DIR)
                .join(name)
                .is_dir()
        })
    {
        cmd.env(LOCALE_FALLBACK_KEY, locale);
    }
}

const REMOTE_CONTEXT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Minimum gap between `tcgetpgrp` calls during a sustained flood.
const GROUP_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// Reads smaller than this, or after a pause, check the foreground group immediately.
const GROUP_FLOOD_BYTES: usize = 512;

/// A `read` that blocked this long counts as a pause, not a flood.
const GROUP_IDLE_GAP: Duration = Duration::from_millis(2);

/// Cap for appending adjacent `Output` into one in-process buffer.
const OUTPUT_COALESCE_MAX: usize = 256 * 1024;

/// One PTY read after draining bytes the kernel has already queued.
const READ_BURST: usize = OUTPUT_COALESCE_MAX;

/// Below this, a read is a keystroke, not a packet worth draining.
#[cfg_attr(not(unix), allow(dead_code))]
const PTY_CHUNK: usize = 256;

pub struct OutputGate {
    queued: AtomicI64,
    park: Mutex<()>,
    drained: Condvar,
}

impl OutputGate {
    const HIGH_WATER: i64 = 16 * 1024 * 1024;
    const MAX_WAIT: Duration = Duration::from_secs(2);

    pub(crate) fn new() -> Self {
        Self {
            queued: AtomicI64::new(0),
            park: Mutex::new(()),
            drained: Condvar::new(),
        }
    }

    fn add(&self, n: usize) {
        self.queued.fetch_add(n as i64, Ordering::Relaxed);
    }

    pub fn sub(&self, n: usize) {
        let prev = self.queued.fetch_sub(n as i64, Ordering::Relaxed);
        if prev >= Self::HIGH_WATER && prev - (n as i64) < Self::HIGH_WATER {
            let _park = self.park.lock().unwrap();
            self.drained.notify_all();
        }
    }

    fn reset(&self) {
        self.queued.store(0, Ordering::Relaxed);
        let _park = self.park.lock().unwrap();
        self.drained.notify_all();
    }

    fn wait_below_high_water(&self) {
        if self.queued.load(Ordering::Relaxed) < Self::HIGH_WATER {
            return;
        }
        let deadline = std::time::Instant::now() + Self::MAX_WAIT;
        let mut park = self.park.lock().unwrap();
        while self.queued.load(Ordering::Relaxed) >= Self::HIGH_WATER {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            let (guard, _) = self.drained.wait_timeout(park, left).unwrap();
            park = guard;
        }
    }
}

/// In-process byte pipe from the pane reader to the terminal reader.
///
/// `std::sync::mpsc` delivers every `Output` as its own allocation and wakes
/// the consumer on every send. On a burst the PTY returns ~1 KiB per read, so
/// that is tens of thousands of wakes. This queue appends an `Output` onto the
/// `Output` already at the back (up to [`OUTPUT_COALESCE_MAX`]) and notifies
/// only when the queue goes from empty to non-empty. A `Cwd`, `Image`, or
/// `Exited` is its own element, so it stays in order relative to the bytes
/// before and after it — a kitty image is anchored at the cursor those bytes
/// leave behind.
struct OutputPipe {
    state: Mutex<OutputState>,
    cv: Condvar,
}

struct OutputState {
    queue: VecDeque<DaemonMsg>,
    /// Capacity returned by the consumer after it copied a batch out.
    spare: Vec<u8>,
    closed: bool,
}

/// Sending half. Dropping it closes the pipe.
pub struct OutputTx {
    pipe: Arc<OutputPipe>,
}

/// Receiving half.
pub struct OutputRx {
    pipe: Arc<OutputPipe>,
}

pub fn output_pipe() -> (OutputTx, OutputRx) {
    let pipe = Arc::new(OutputPipe {
        state: Mutex::new(OutputState {
            queue: VecDeque::new(),
            spare: Vec::with_capacity(OUTPUT_COALESCE_MAX),
            closed: false,
        }),
        cv: Condvar::new(),
    });
    (
        OutputTx {
            pipe: Arc::clone(&pipe),
        },
        OutputRx { pipe },
    )
}

impl OutputTx {
    /// Queue `msg`, folding an `Output` into the previous one when that is
    /// also output and still under the cap.
    pub fn push(&self, msg: DaemonMsg) -> bool {
        match msg {
            DaemonMsg::Output(bytes) => self.append(&bytes),
            other => self.push_other(other),
        }
    }

    /// Copy `bytes` onto the coalesced output buffer. No per-chunk `Vec`.
    pub fn append(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let mut guard = match self.pipe.state.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        if guard.closed {
            return false;
        }
        let was_empty = guard.queue.is_empty();
        let mut rest = bytes;
        while !rest.is_empty() {
            let fitted = match guard.queue.back_mut() {
                Some(DaemonMsg::Output(buf)) if buf.len() < OUTPUT_COALESCE_MAX => {
                    let n = rest.len().min(OUTPUT_COALESCE_MAX - buf.len());
                    buf.extend_from_slice(&rest[..n]);
                    rest = &rest[n..];
                    true
                }
                _ => false,
            };
            if fitted {
                continue;
            }
            let n = rest.len().min(OUTPUT_COALESCE_MAX);
            let mut buf = std::mem::take(&mut guard.spare);
            buf.clear();
            buf.extend_from_slice(&rest[..n]);
            guard.queue.push_back(DaemonMsg::Output(buf));
            rest = &rest[n..];
        }
        if was_empty {
            self.pipe.cv.notify_one();
        }
        true
    }

    fn push_other(&self, msg: DaemonMsg) -> bool {
        let mut guard = match self.pipe.state.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        if guard.closed {
            return false;
        }
        let was_empty = guard.queue.is_empty();
        guard.queue.push_back(msg);
        if was_empty {
            self.pipe.cv.notify_one();
        }
        true
    }
}

impl Drop for OutputTx {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.pipe.state.lock() {
            guard.closed = true;
            self.pipe.cv.notify_all();
        }
    }
}

impl OutputRx {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<DaemonMsg, mpsc::RecvTimeoutError> {
        let Ok(mut guard) = self.pipe.state.lock() else {
            return Err(mpsc::RecvTimeoutError::Disconnected);
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(msg) = guard.queue.pop_front() {
                return Ok(msg);
            }
            if guard.closed {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            let (next, result) = self.pipe.cv.wait_timeout(guard, left).unwrap();
            guard = next;
            if result.timed_out() && guard.queue.is_empty() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
        }
    }

    pub fn try_recv(&self) -> Result<DaemonMsg, mpsc::TryRecvError> {
        let Ok(mut guard) = self.pipe.state.lock() else {
            return Err(mpsc::TryRecvError::Disconnected);
        };
        if let Some(msg) = guard.queue.pop_front() {
            return Ok(msg);
        }
        if guard.closed {
            Err(mpsc::TryRecvError::Disconnected)
        } else {
            Err(mpsc::TryRecvError::Empty)
        }
    }

    pub fn recv(&self) -> Result<DaemonMsg, mpsc::RecvError> {
        let Ok(mut guard) = self.pipe.state.lock() else {
            return Err(mpsc::RecvError);
        };
        loop {
            if let Some(msg) = guard.queue.pop_front() {
                return Ok(msg);
            }
            if guard.closed {
                return Err(mpsc::RecvError);
            }
            guard = self.pipe.cv.wait(guard).unwrap();
        }
    }

    /// Drain whatever is queued without waiting. Tests use this the way they
    /// used `mpsc::Receiver::try_iter`.
    #[cfg(test)]
    fn try_iter(&self) -> impl Iterator<Item = DaemonMsg> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// Hand a drained output buffer back so the next batch reuses its capacity.
    pub fn recycle(&self, mut buf: Vec<u8>) {
        if buf.capacity() < 4096 {
            return;
        }
        buf.clear();
        let Ok(mut guard) = self.pipe.state.lock() else {
            return;
        };
        if buf.capacity() > guard.spare.capacity() {
            guard.spare = buf;
        }
    }
}

/// How long the death path will chase a child's exit status before giving up
/// and reporting `Exited { code: None }`. The pty hits EOF when the last slave
/// fd closes, which can land a few milliseconds ahead of the child becoming
/// reapable — that race is all this window exists to cover. Everything past it
/// is a pane that looks frozen to every attached client, so it stays short.
const EXIT_CODE_PROBE_WINDOW: Duration = Duration::from_millis(500);

const EXIT_CODE_PROBE_INTERVAL: Duration = Duration::from_millis(10);

struct PaneState {
    id: u64,
    /// Geometry the pty is at. Output bytes are not retained.
    size: WinSize,
    subscriber: Option<OutputTx>,
    cwd: Option<PathBuf>,
    shell: ShellState,
    remote: Option<RemoteContext>,
    agent: Option<crate::core::cli_agent::CLIAgent>,
    agent_argv: Option<Vec<String>>,
    agent_session: Option<crate::core::cli_agent::AgentSessionState>,
    agent_clock: AgentClock,
    alive: bool,
}

/// Whether a pending interrupt key is still waiting, and which generation of
/// hook events it was armed against.
#[derive(Default)]
struct AgentClock {
    /// Bumped by every hook event and every session reset, so a pending
    /// interrupt can tell whether anything spoke after the key it was armed by.
    generation: u64,
    /// An interrupt key is already waiting out [`INTERRUPT_SETTLE`].
    interrupt_pending: bool,
}

/// How long after an interrupt key a real hook event still gets to speak
/// first. Agents that do report an interrupt (Kimi's `Interrupt`, a `Stop`
/// racing the key) do it well inside this.
const INTERRUPT_SETTLE: Duration = Duration::from_millis(1000);

/// Whether one input write is the user asking a running agent to stop: a bare
/// <kbd>Esc</kbd> or <kbd>Ctrl+C</kbd>, in the legacy encoding or in the
/// kitty keyboard protocol and xterm `modifyOtherKeys` forms a TUI may have
/// switched the terminal into. Only a whole write counts — the key arrives on
/// its own, and a paste that happens to hold `0x03` is not a keypress.
#[cfg(test)]
fn is_interrupt_key(bytes: &[u8]) -> bool {
    interrupt_key(bytes).is_some()
}

/// Which interrupt key one input write is — see [`is_interrupt_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptKey {
    Escape,
    CtrlC,
}

/// Whether `bytes` stops `agent`'s turn. Grok Build takes only Ctrl+C for
/// that: its Esc leaves a running turn running, and counting it would call
/// the turn over while it still works.
fn interrupts(agent: Option<crate::core::cli_agent::CLIAgent>, bytes: &[u8]) -> bool {
    match interrupt_key(bytes) {
        Some(InterruptKey::Escape) => agent != Some(crate::core::cli_agent::CLIAgent::Grok),
        Some(InterruptKey::CtrlC) => true,
        None => false,
    }
}

fn interrupt_key(bytes: &[u8]) -> Option<InterruptKey> {
    match bytes {
        b"\x1b" => return Some(InterruptKey::Escape),
        b"\x03" | b"\x1b[27;5;99~" => return Some(InterruptKey::CtrlC),
        _ => {}
    }
    let body = bytes
        .strip_prefix(b"\x1b[")
        .and_then(|b| b.strip_suffix(b"u"))
        .and_then(|b| std::str::from_utf8(b).ok())?;
    let mut fields = body.split(';');
    let code = fields
        .next()
        .and_then(|f| f.split(':').next())
        .and_then(|c| c.parse::<u32>().ok())?;
    let mut modifiers = fields.next().unwrap_or("1").split(':');
    let mods = modifiers
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(1);
    let event = modifiers.next().map_or(Some(1), |e| e.parse::<u32>().ok());
    if event != Some(1) {
        return None;
    }
    // Caps Lock and Num Lock ride along in the mask without changing the key.
    const LOCKS: u32 = 64 | 128;
    let mods = mods.saturating_sub(1) & !LOCKS;
    match (code, mods) {
        (27, 0) => Some(InterruptKey::Escape),
        (99, 4) => Some(InterruptKey::CtrlC),
        _ => None,
    }
}

fn notify(st: &mut PaneState, msg: DaemonMsg) {
    if let Some(sub) = &st.subscriber {
        sub.push(msg);
    }
}

/// The context the pane is born with, sent before the reader exists.
///
/// `RemoteContext` first. The client clears its cwd on that frame, so the
/// directory has to follow or a WSL pane, and any pane whose first probe
/// matches this path, never delivers the directory it started in.
fn publish_standing(st: &PaneState) {
    let Some(sub) = &st.subscriber else {
        return;
    };
    if st.remote.is_some() {
        sub.push(DaemonMsg::RemoteContext(st.remote.clone()));
    }
    if let Some(cwd) = &st.cwd {
        sub.push(DaemonMsg::Cwd(cwd.clone()));
    }
}

/// Apply a resize to the pane's shared state and echo `Size` to the subscriber.
///
/// The controller's `Size` echo has to be sent while the state lock is held —
/// output is fanned out under the same lock, so the frame lands in the stream
/// after every byte produced at the old size and before the repaint the PTY
/// emits once it is actually resized (which happens after the lock is
/// released). The client defers its grid reflow to this marker instead of
/// reflowing the moment its window changed: the channel can hold megabytes
/// of old-width output (the pane gate allows
/// 16 MiB), and reflowing ahead of it parsed old-width bytes into a new-width
/// grid, garbling the pane on maximize during a burst of output.
///
/// The marker is deliberately not airtight. Old-width bytes the reader thread
/// has read but not yet fanned out — and bytes still sitting in the kernel
/// pipe, which conhost keeps producing until it processes the resize — land
/// after the echo and parse at the new width. That residue is bounded by one
/// [`READ_BURST`] plus the pipe, cannot be attributed to a geometry from here
/// (the bytes carry no tags and ConPTY emits no sync marker of its own), and
/// is the same ambiguity every ConPTY terminal accepts on a mid-output
/// resize. The echo exists to close the 16 MiB window, not that one.
fn resize_state(st: &mut PaneState, size: WinSize) {
    st.size = size;
    if let Some(sub) = &st.subscriber {
        sub.push(DaemonMsg::Size(size));
    }
}

/// One gated message to the subscriber.
///
/// A send error just means the subscriber is gone. A successful send is
/// counted against the pane gate; the reader credits it back once the bytes
/// have been taken.
fn fan_out_one(st: &mut PaneState, msg: DaemonMsg, len: usize, gate: &OutputGate) {
    if let Some(sub) = &st.subscriber
        && sub.push(msg)
    {
        gate.add(len);
    }
}

/// Fan one PTY read out to the subscriber.
///
/// On the no-graphics fast path `frames` is empty and the whole passthrough
/// goes as a single `Output`. When a chunk carried graphics the frames are
/// forwarded in stream order instead, so an image lands at the same cursor cell
/// the sender drew it at. Each `Output` is gated on its own length; an image
/// frame is gated too (it rode the same PTY read); a delete is tiny and
/// ungated, matching the drain accounting in `server.rs`.
fn fan_out_output(st: &mut PaneState, bytes: &[u8], frames: Vec<GraphicsFrame>, gate: &OutputGate) {
    if frames.is_empty() {
        if bytes.is_empty() {
            return;
        }
        // The coalesced pipe appends into the buffer already queued. Building
        // a `Vec` here would put the 1 KiB allocation back on the hot path.
        if let Some(sub) = &st.subscriber
            && sub.append(bytes)
        {
            gate.add(bytes.len());
        }
        return;
    }
    for frame in frames {
        match frame {
            GraphicsFrame::Output(b) => {
                if !b.is_empty() {
                    let len = b.len();
                    fan_out_one(st, DaemonMsg::Output(b), len, gate);
                }
            }
            GraphicsFrame::Image(frame) => {
                let len = frame.len();
                fan_out_one(st, DaemonMsg::Image(frame), len, gate);
            }
            GraphicsFrame::Delete(sel) => notify(st, DaemonMsg::DeleteImage(sel)),
        }
    }
}

struct ForegroundProbes {
    /// Foreground process group. A change means the probes below would now answer differently.
    group: Box<dyn Fn() -> Option<i32> + Send>,
    remote: Box<dyn Fn() -> Option<RemoteContext> + Send>,
    agent: Box<dyn Fn() -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> + Send>,
    cwd: Box<dyn Fn() -> Option<PathBuf> + Send>,
}

struct PtyBackend {
    /// Owns the PTY master until the pane is closed.
    ///
    /// Windows takes this value when the shell process exits. Dropping the
    /// ConPTY master calls `ClosePseudoConsole`, which closes the output side
    /// only after its pending bytes can be consumed by the dedicated reader.
    /// Keeping the slot optional lets that monitor end the stream without
    /// racing the reader for ownership of the exit notification.
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    #[cfg_attr(windows, allow(dead_code))]
    shell_pid: Option<u32>,
    integration_dir: Option<PathBuf>,
}

/// The pty half of a pane, before it is wired up: opened and spawned into by
/// [`DaemonPane::spawn`], inherited across an `exec` by [`DaemonPane::adopt`].
struct PtyParts {
    master: Box<dyn MasterPty + Send>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    shell_pid: Option<u32>,
    integration_dir: Option<PathBuf>,
    reader_handle: Box<dyn Read + Send>,
    writer: Arc<PtyWriter>,
}

pub struct DaemonPane {
    pub id: u64,
    backend: PtyBackend,
    /// Keyboard input and in-band replies (kitty query acknowledgements,
    /// clipboard errors, detached device-attribute answers). A thread owns
    /// the fd: [`PtyWriter::write`] only enqueues, so a PTY that has stopped
    /// reading cannot block the UI thread or the reader.
    writer: Arc<PtyWriter>,
    /// Set during teardown so the reader doesn't emit a spurious exit.
    shutting_down: Arc<AtomicBool>,
    gate: Arc<OutputGate>,
    state: Arc<Mutex<PaneState>>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

/// Logged when queued input crosses this size. Writes are not dropped: a
/// paste split across `write` calls has to keep its closing `\x1b[201~`, and
/// one in-flight query reply must not throw the paste away. The queue is
/// abandoned only when the pane is closing or the pty write fails.
const PTY_WRITE_BOUND: usize = 64 << 20;

struct PtyQueue {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    closing: bool,
    /// Set when the writer thread is outside `write_all` and `chunks` is empty.
    /// Tests wait on it. Production only writes it.
    #[cfg_attr(not(test), allow(dead_code))]
    idle: bool,
}

/// Owns the PTY master writer on a background thread.
///
/// `write` copies the bytes onto a bounded queue and returns. A foreground
/// program that has stopped reading — XOFF, a hung child, a paste larger
/// than the kernel's input buffer — blocks that thread inside `write_all`,
/// not the GPUI thread and not the pane reader (kitty and OSC replies use
/// this queue too).
struct PtyWriter {
    queue: Arc<(Mutex<PtyQueue>, Condvar)>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PtyWriter {
    fn spawn(mut sink: Box<dyn Write + Send>) -> Arc<Self> {
        let queue = Arc::new((
            Mutex::new(PtyQueue {
                chunks: VecDeque::new(),
                bytes: 0,
                closing: false,
                idle: true,
            }),
            Condvar::new(),
        ));
        let sending = Arc::clone(&queue);
        let thread = std::thread::Builder::new()
            .name("tty7-pty-writer".to_string())
            .spawn(move || {
                crate::core::threads::promote_to_user_interactive();
                pty_write_loop(&sending, &mut sink);
            })
            .expect("spawn pty writer");
        Arc::new(Self {
            queue,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Queue `bytes` and return. Never blocks on the PTY, and never drops a
    /// chunk: the closing bracket of a paste is often its own write.
    fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let (lock, wake) = &*self.queue;
        let Ok(mut q) = lock.lock() else {
            return;
        };
        if q.closing {
            return;
        }
        let before = q.bytes;
        q.bytes = q.bytes.saturating_add(bytes.len());
        if before < PTY_WRITE_BOUND && q.bytes >= PTY_WRITE_BOUND {
            log::warn!(
                "pty input queue is past {} MiB; the child is not reading",
                PTY_WRITE_BOUND / (1 << 20)
            );
        }
        q.idle = false;
        q.chunks.push_back(bytes.to_vec());
        drop(q);
        wake.notify_all();
    }

    /// Block until queued bytes have been handed to the sink, or `timeout`.
    #[cfg(test)]
    fn wait_idle(&self, timeout: Duration) -> bool {
        let (lock, wake) = &*self.queue;
        let Ok(guard) = lock.lock() else {
            return false;
        };
        match wake.wait_timeout_while(guard, timeout, |q| !q.idle) {
            Ok((q, _)) => q.idle,
            Err(_) => false,
        }
    }

    /// Ask the writer thread to finish. A `write_all` stuck on the PTY is
    /// abandoned after two seconds — the caller, often the UI thread tearing
    /// a pane down, must not wait on that fd. `hangup` has already killed
    /// the child, which is what usually unblocks the write first.
    fn shutdown(&self) {
        let (lock, wake) = &*self.queue;
        if let Ok(mut q) = lock.lock() {
            q.closing = true;
            wake.notify_all();
        }
        let handle = self.thread.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            join_bounded(handle, Duration::from_secs(2));
        }
    }
}

impl Drop for PtyWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn pty_write_loop(queue: &Arc<(Mutex<PtyQueue>, Condvar)>, sink: &mut Box<dyn Write + Send>) {
    let (lock, wake) = &**queue;
    loop {
        let chunks = {
            let Ok(mut q) = lock.lock() else {
                return;
            };
            loop {
                if !q.chunks.is_empty() {
                    break;
                }
                q.idle = true;
                wake.notify_all();
                if q.closing {
                    return;
                }
                q = match wake.wait(q) {
                    Ok(next) => next,
                    Err(_) => return,
                };
            }
            q.idle = false;
            std::mem::take(&mut q.chunks)
        };
        for chunk in &chunks {
            if sink.write_all(chunk).is_err() {
                abandon_pty_queue(queue);
                return;
            }
            // Count the chunk only after it is in the pty, so a write that
            // arrives while this one is blocked is not treated as the whole
            // queue having already drained.
            let Ok(mut q) = lock.lock() else {
                abandon_pty_queue(queue);
                return;
            };
            q.bytes = q.bytes.saturating_sub(chunk.len());
        }
        if sink.flush().is_err() {
            abandon_pty_queue(queue);
            return;
        }
    }
}

fn abandon_pty_queue(queue: &Arc<(Mutex<PtyQueue>, Condvar)>) {
    let (lock, wake) = &**queue;
    if let Ok(mut q) = lock.lock() {
        q.chunks.clear();
        q.bytes = 0;
        q.idle = true;
        q.closing = true;
    }
    wake.notify_all();
}

struct DeathReporter {
    reported: AtomicBool,
    on_dead: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    exit_code: Mutex<Option<Box<dyn FnMut() -> Option<i32> + Send>>>,
}

impl DeathReporter {
    fn new(on_dead: impl FnOnce() + Send + 'static) -> Self {
        Self {
            reported: AtomicBool::new(false),
            on_dead: Mutex::new(Some(Box::new(on_dead))),
            exit_code: Mutex::new(None),
        }
    }

    fn probe_exit_code(&self, probe: impl FnMut() -> Option<i32> + Send + 'static) {
        *self.exit_code.lock().unwrap() = Some(Box::new(probe));
    }

    #[cfg(windows)]
    fn has_reported(&self) -> bool {
        self.reported.load(Ordering::SeqCst)
    }

    fn report(&self, state: &Mutex<PaneState>, shutting_down: &AtomicBool) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let code = self
            .exit_code
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|probe| probe());
        let mut st = state.lock().unwrap();
        st.alive = false;
        let pane = st.id;
        log::info!(
            "pane {pane} exited; exit_code={code:?} shutdown_requested={}",
            shutting_down.load(Ordering::SeqCst)
        );
        if shutting_down.load(Ordering::SeqCst) {
            drop(st);
            return;
        }
        let subscribed = st.subscriber.is_some();
        notify(&mut st, DaemonMsg::Exited { code });
        drop(st);
        if subscribed {
            return;
        }
        if let Some(on_dead) = self.on_dead.lock().unwrap().take() {
            on_dead();
        }
    }
}

/// Releases the PTY master without holding its slot mutex during destruction.
///
/// `ClosePseudoConsole` may wait while the output pipe is drained, and the same
/// slot is what `resize` locks. Dropping the master only after releasing the
/// mutex keeps a concurrent resize from parking behind a close that is itself
/// waiting on the reader.
#[cfg(windows)]
fn close_pty_master(master: &Mutex<Option<Box<dyn MasterPty + Send>>>) {
    let owned = master.lock().ok().and_then(|mut slot| slot.take());
    drop(owned);
}

/// How long the Windows exit monitor lets the reader announce the death on its
/// own before doing it itself.
///
/// The reader is the better reporter: it publishes `Exited` only after it has
/// forwarded every byte that preceded EOF, which is what keeps a short-lived
/// command's final frame ahead of its exit. But EOF is not guaranteed. A
/// grandchild that inherited the ConPTY output pipe holds it open after the
/// shell is gone — `cmd /c start …` is enough — and `ClosePseudoConsole` then
/// never completes. Without this window such a pane would read as alive
/// forever to every attached client.
#[cfg(windows)]
const EXIT_DRAIN_WINDOW: Duration = Duration::from_secs(2);

#[cfg(windows)]
const EXIT_DRAIN_POLL: Duration = Duration::from_millis(10);

/// Releases the pseudoconsole, then reports the pane's death — preferring the
/// reader's EOF-ordered report and falling back to its own after `window`.
///
/// Must run on a background thread: both halves block.
#[cfg(windows)]
fn drain_then_report(
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    state: Arc<Mutex<PaneState>>,
    shutting_down: Arc<AtomicBool>,
    death: Arc<DeathReporter>,
    window: Duration,
) {
    // `ClosePseudoConsole` can block until the reader drains the pipe, so it
    // cannot run on the thread that owns the deadline below.
    std::thread::Builder::new()
        .name("tty7-daemon-pane-pty-close".to_string())
        .spawn(move || close_pty_master(&master))
        .expect("spawn daemon pane pty close thread");

    let deadline = std::time::Instant::now() + window;
    while !death.has_reported() && std::time::Instant::now() < deadline {
        std::thread::sleep(EXIT_DRAIN_POLL);
    }
    // `DeathReporter` is idempotent, so this is a no-op whenever the reader
    // already got there — which is the ordinary case.
    death.report(&state, &shutting_down);
}

/// One out-of-band frame the reader forwards to the subscriber, kept in stream
/// order so a kitty image lands at the cursor cell the sender drew it at: a chunk
/// carrying graphics splits into `Output` runs interleaved with `Image`/`Delete`
/// frames, and they must reach the client in that same order. `Output` becomes a
/// `DaemonMsg::Output`, `Image` an encoded image frame, `Delete` a compact
/// selector — see the reader loop's `sniff` handling.
#[derive(Debug)]
enum GraphicsFrame {
    Output(Vec<u8>),
    Image(Vec<u8>),
    Delete(Vec<u8>),
}

/// Queue an encoded image frame for the subscriber, dropping any frame larger
/// than [`MAX_FRAME`]. An oversize image must not enter the subscriber queue:
/// one frame that will not fit is not worth stalling the rest of the stream.
/// Drop it here so output keeps flowing. The inline
/// `t=d` path is bounded by `MAX_TRANSMISSION_BASE64` but that still admits
/// ~72 MiB of raw pixels, and the local shm/file path has no cap of its own,
/// so both can land here over the limit.
fn push_image_frame(frames: &mut Vec<GraphicsFrame>, frame: Vec<u8>) {
    if frame.len() <= MAX_FRAME {
        frames.push(GraphicsFrame::Image(frame));
    }
}

fn process_graphics_output(
    sniffed: Sniffed<'_>,
    ordered: bool,
    pass: &mut Vec<u8>,
    frames: &mut Vec<GraphicsFrame>,
    writer: &PtyWriter,
) {
    match sniffed {
        Sniffed::Plain(bytes) => {
            pass.extend_from_slice(bytes);
            if ordered && !bytes.is_empty() {
                frames.push(GraphicsFrame::Output(bytes.to_vec()));
            }
        }
        Sniffed::Segments(segments) => {
            for segment in segments {
                match segment {
                    Segment::Output(bytes) => {
                        pass.extend_from_slice(&bytes);
                        frames.push(GraphicsFrame::Output(bytes));
                    }
                    Segment::Query(reply) => {
                        writer.write(&reply);
                    }
                    Segment::Image(image) => {
                        push_image_frame(frames, image.encode_frame());
                    }
                    Segment::ImageFromMedium(transfer) => {
                        if let Some(image) = transfer.resolve() {
                            push_image_frame(frames, image.encode_frame());
                        }
                    }
                    Segment::Delete(delete) => {
                        frames.push(GraphicsFrame::Delete(delete.encode()));
                    }
                }
            }
        }
    }
}

/// Strip OSC 5522 clipboard writes and Kitty graphics out of one PTY read
/// before anything else sees it. On the common path the original slice is
/// borrowed unchanged. A clipboard write is answered on the pty (`EPERM` for
/// a write, `ENOSYS` for a read) and never forwarded. Kitty images become
/// out-of-band frames and never enter the passthrough.
///
/// `frames` receives the ordered list of out-of-band frames to forward *in
/// stream position*: a kitty image anchors to the cursor cell as it stood when
/// its command appeared, so the client must apply the text before an image,
/// then the image, then the text after it, in that order. On the no-graphics
/// fast path `frames` stays empty and the whole chunk is sent as one `Output`;
/// only a chunk with graphics splits into interleaved frames.
fn lift_out_of_band<'a>(
    raw: &'a [u8],
    clipboard: &mut ClipboardSniffer,
    graphics: &mut GraphicsSniffer,
    writer: &PtyWriter,
    frames: &mut Vec<GraphicsFrame>,
) -> std::borrow::Cow<'a, [u8]> {
    match clipboard.sniff(raw) {
        ClipboardSniffed::Plain(b) => match graphics.sniff(b) {
            Sniffed::Plain(b) => std::borrow::Cow::Borrowed(b),
            sniffed @ Sniffed::Segments(_) => {
                let mut pass = Vec::new();
                process_graphics_output(sniffed, false, &mut pass, frames, writer);
                std::borrow::Cow::Owned(pass)
            }
        },
        ClipboardSniffed::Segments(segs) => {
            let mut pass = Vec::new();
            for seg in segs {
                match seg {
                    ClipboardSegment::Output(b) => {
                        let sniffed = graphics.sniff(&b);
                        process_graphics_output(sniffed, true, &mut pass, frames, writer);
                    }
                    ClipboardSegment::Reply(reply) => {
                        writer.write(&reply);
                    }
                }
            }
            std::borrow::Cow::Owned(pass)
        }
    }
}

/// Split one chunk into passthrough, out-of-band frames, and OSC signals.
/// Ground machines with no `0x1b` skip the parsers; a sequence split across reads stays outside ground.
fn scan_chunk<'a>(
    raw: &'a [u8],
    clipboard: &mut ClipboardSniffer,
    graphics: &mut GraphicsSniffer,
    sniffer: &mut OscSniffer,
    writer: &PtyWriter,
) -> (std::borrow::Cow<'a, [u8]>, Vec<GraphicsFrame>, SniffSignals) {
    let has_esc = crate::core::osc::contains_esc(raw);
    let mut frames = Vec::new();
    let passthrough = if !has_esc && clipboard.is_ground() && graphics.is_ground() {
        std::borrow::Cow::Borrowed(raw)
    } else {
        lift_out_of_band(raw, clipboard, graphics, writer, &mut frames)
    };
    let out_has_esc = match &passthrough {
        std::borrow::Cow::Borrowed(_) if !has_esc => false,
        bytes => crate::core::osc::contains_esc(bytes),
    };
    let signals = if !out_has_esc && sniffer.is_ground() {
        SniffSignals::default()
    } else {
        sniffer.feed(&passthrough)
    };
    (passthrough, frames, signals)
}

/// After a full packet, drains output the kernel already queued, up to [`READ_BURST`].
/// A short read returns immediately, and the master stays blocking: `dup` shares the fd with [`PtyWriter`].
struct BurstRead {
    inner: Box<dyn Read + Send>,
    poll_fd: Option<i32>,
}

impl BurstRead {
    fn boxed(inner: Box<dyn Read + Send>, poll_fd: Option<i32>) -> Box<dyn Read + Send> {
        Box::new(Self { inner, poll_fd })
    }
}

impl Drop for BurstRead {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(fd) = self.poll_fd.take() {
            unsafe { libc::close(fd) };
        }
    }
}

impl Read for BurstRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // The first read is the one that blocks, and it is a fresh ask on
        // every later round. On Darwin a larger ask makes `ONLCR` insert a
        // CR when a newline sits on a 1024-byte boundary, so it has to be
        // capped the same way as the drain that follows. Elsewhere
        // `drain_read_len` is the length unchanged.
        let filled = {
            #[cfg(unix)]
            {
                if self.poll_fd.is_some() {
                    let first_want = drain_read_len(buf.len());
                    self.inner.read(&mut buf[..first_want])?
                } else {
                    self.inner.read(buf)?
                }
            }
            #[cfg(not(unix))]
            {
                self.inner.read(buf)?
            }
        };
        if filled == 0 || filled == buf.len() {
            return Ok(filled);
        }
        // A short read is the whole delivery: draining it is another syscall, and the only read that can block with bytes in hand.
        #[cfg(unix)]
        if filled >= PTY_CHUNK
            && let Some(fd) = self.poll_fd
        {
            return drain_queued(&mut *self.inner, fd, buf, filled);
        }
        #[cfg(not(unix))]
        let _ = self.poll_fd;
        Ok(filled)
    }
}

/// While `POLLIN` is set, read the next packet. `XOFF` clears `POLLIN` while `TIOCOUTQ` stays positive, so a stopped queue is not read.
#[cfg(unix)]
fn drain_queued(
    inner: &mut dyn Read,
    fd: i32,
    buf: &mut [u8],
    mut filled: usize,
) -> std::io::Result<usize> {
    while filled < buf.len() && pty_readable(fd) {
        let want = drain_read_len(buf.len() - filled);
        match inner.read(&mut buf[filled..filled + want]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            // Keep the bytes already in hand. The next `read` reports the error.
            Err(_) => break,
        }
    }
    Ok(filled)
}

/// Darwin returns at most one 1024-byte clist block; a larger ask makes `ONLCR` insert a CR.
#[cfg(unix)]
fn drain_read_len(remaining: usize) -> usize {
    #[cfg(target_vendor = "apple")]
    return remaining.min(1024);
    #[cfg(not(target_vendor = "apple"))]
    remaining
}

/// Zero-timeout `POLLIN`. It does not consume the byte a later `read` returns.
#[cfg(unix)]
fn pty_readable(fd: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        return rc > 0 && (pfd.revents & libc::POLLIN) != 0;
    }
}

/// Slave bytes waiting on the master, for tests. `None` if the queue is empty or the ioctl failed.
#[cfg(all(unix, test))]
fn queued_pty_output(fd: i32) -> Option<usize> {
    let mut avail: libc::c_int = 0;
    // macOS counts readable output with `TIOCOUTQ`; `FIONREAD` there is unread keystrokes. Linux uses `FIONREAD`.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    let req = libc::TIOCOUTQ;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )))]
    let req = libc::FIONREAD;
    loop {
        let rc = unsafe { libc::ioctl(fd, req, &mut avail) };
        if rc >= 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
    (avail > 0).then_some(avail as usize)
}

#[cfg(unix)]
fn dup_reader_fd(master: &dyn MasterPty) -> Option<i32> {
    let fd = master.as_raw_fd()?;
    let duped = unsafe { libc::dup(fd) };
    if duped < 0 {
        return None;
    }
    unsafe {
        libc::fcntl(duped, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Some(duped)
}

#[cfg(not(unix))]
fn dup_reader_fd(_master: &dyn MasterPty) -> Option<i32> {
    None
}

impl DaemonPane {
    /// Open a pane whose subscriber is installed before the reader thread
    /// exists, so the first byte of output is delivered live.
    ///
    /// `use_config_shell` is false when a broken `shell.program` has already
    /// failed and the login shell should be used instead.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_subscribed(
        id: u64,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        use_config_shell: bool,
        subscriber: OutputTx,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open(
            id,
            cwd,
            size,
            shell,
            use_config_shell,
            Some(subscriber),
            on_dead,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open(
        id: u64,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        use_config_shell: bool,
        subscriber: Option<OutputTx>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let pty_size = pty_size(size);

        // Before the shell exists, not after: a ConPTY child inherits the
        // "ignore Ctrl+C" state of whoever created it, and the daemon can be
        // carrying it from its own launch. Leaving it on costs the pane every
        // Ctrl+C it will ever be sent (#451, #314).
        #[cfg(windows)]
        crate::daemon::winproc::allow_ctrl_c_in_children();

        let pair = native_pty_system().openpty(pty_size)?;
        let spawn = build_spawn_config(id, cwd, shell, use_config_shell)?;

        let child = pair.slave.spawn_command(spawn.cmd)?;
        let shell_pid = child.process_id();
        log::info!("pane {id} child started; child_pid={shell_pid:?}");
        let child = Arc::new(Mutex::new(child));

        drop(pair.slave);

        let reader_handle = pair.master.try_clone_reader()?;
        let writer = PtyWriter::spawn(pair.master.take_writer()?);

        let mut state = PaneState {
            id,
            size,
            subscriber: None,
            cwd: spawn.initial_cwd,
            shell: ShellState::default(),
            remote: spawn.remote.clone(),
            agent: None,
            agent_session: None,
            agent_argv: None,
            agent_clock: AgentClock::default(),
            alive: true,
        };
        if let Some(subscriber) = subscriber {
            // Installed before the reader exists, so the first output byte is
            // delivered live. The standing context goes out first: the client
            // drops its cwd on every `RemoteContext`, and a later probe that
            // finds the same directory will not send it again.
            state.subscriber = Some(subscriber);
            publish_standing(&state);
        }

        Ok(Self::over_pty(
            PtyParts {
                master: pair.master,
                child,
                shell_pid,
                integration_dir: spawn.integration_dir,
                reader_handle,
                writer,
            },
            state,
            on_dead,
        ))
    }

    /// Wire a pty, a child and a starting state into a running pane.
    ///
    /// Shared by the two ways a pty-backed pane comes to exist — one that opens
    /// a pty and starts a shell in it, and one that inherits both from the
    /// image it replaced. Everything below this line is identical for them, and
    /// it is the part where getting it subtly wrong shows up as a pane that
    /// never reports its death or never sees its own output.
    fn over_pty(
        parts: PtyParts,
        state: PaneState,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> Arc<Self> {
        let PtyParts {
            master,
            child,
            shell_pid,
            integration_dir,
            reader_handle,
            writer,
        } = parts;
        let reader_handle = BurstRead::boxed(reader_handle, dup_reader_fd(master.as_ref()));

        let id = state.id;
        let state = Arc::new(Mutex::new(state));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());
        let master = Arc::new(Mutex::new(Some(master)));

        let pane = Arc::new(Self {
            id,
            backend: PtyBackend {
                master: master.clone(),
                child: child.clone(),
                shell_pid,
                integration_dir,
            },
            writer: writer.clone(),
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
        });

        let death = Arc::new(DeathReporter::new(on_dead));
        death.probe_exit_code({
            let child = child.clone();
            move || {
                let deadline = std::time::Instant::now() + EXIT_CODE_PROBE_WINDOW;
                loop {
                    // try_lock, never lock: DaemonPane::drop holds this very
                    // mutex across a blocking child.wait(), and this probe runs
                    // on the reader thread on its way to announcing the death.
                    // Blocking for the lock would park the Exited notification
                    // behind a wait() with no bound of its own; the deadline
                    // below is the only thing clients should ever wait on.
                    if let Ok(mut child) = child.try_lock() {
                        match child.try_wait() {
                            Ok(Some(status)) => return Some(status.exit_code() as i32),
                            Ok(None) => {}
                            Err(_) => return None,
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(EXIT_CODE_PROBE_INTERVAL);
                }
            }
        });

        #[cfg(windows)]
        Self::spawn_exit_monitor(
            shell_pid,
            master.clone(),
            state.clone(),
            pane.shutting_down.clone(),
            death.clone(),
        );

        let fg_master = master.clone();
        let group_master = master.clone();
        let remote_master = master.clone();
        let agent_master = master.clone();
        let cwd_master = master.clone();
        let reader = Self::spawn_reader(
            state,
            shutting_down,
            gate,
            reader_handle,
            writer.clone(),
            move || foreground_command_running(&fg_master, shell_pid),
            ForegroundProbes {
                group: Box::new(move || pty_foreground_pgid(&group_master)),
                remote: Box::new(move || foreground_remote_context(&remote_master)),
                agent: Box::new(move || foreground_agent(&agent_master)),
                cwd: Box::new(move || foreground_cwd(&cwd_master, shell_pid)),
            },
            death,
        );
        *pane.reader.lock().unwrap() = Some(reader);

        pane
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_reader(
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        gate: Arc<OutputGate>,
        mut reader: Box<dyn Read + Send>,
        // The pane's input side, shared so the reader can queue kitty-graphics
        // query replies (`\x1b_G…;OK\x1b\\`) without touching the PTY fd — see
        // the graphics sniff in the loop below.
        writer: Arc<PtyWriter>,
        foreground_running: impl Fn() -> bool + Send + 'static,
        probes: ForegroundProbes,
        death: Arc<DeathReporter>,
    ) -> JoinHandle<()> {
        let ForegroundProbes {
            group: foreground_group,
            remote: foreground_remote,
            agent: foreground_agent_fn,
            cwd: foreground_cwd_fn,
        } = probes;
        std::thread::Builder::new()
            .name("tty7-daemon-pane-reader".to_string())
            .spawn(move || {
                crate::core::threads::promote_to_user_interactive();
                let mut sniffer = OscSniffer::new();
                let mut clipboard = ClipboardSniffer::default();
                // Kitty graphics interception (issue #213): lifts image
                // sequences out of the stream *before* the subscriber sees
                // them, so the base64 pixels never enter replay and the client's
                // VT parser never chews through them. Zero-copy on the common
                // no-graphics chunk — see [`GraphicsSniffer::sniff`].
                //
                // File/shm transfer (`t=s`/`t=f`/`t=t`) is honored only while the
                // pane is *local*: the object/path names resolve on this host, and
                // reading them can't leak across an SSH tunnel. A pane that starts
                // local can `ssh` out mid-session, so the flag is refreshed from
                // the same remote-context poll below. Seed it from the pane's
                // current context so the first probe answers correctly.
                let (starts_local, mut remote_managed) = {
                    let st = state.lock().unwrap();
                    (
                        st.remote.is_none(),
                        st.remote
                            .as_ref()
                            .is_some_and(|remote| remote.kind != RemoteKind::Ssh),
                    )
                };
                let mut graphics = GraphicsSniffer::new_local(starts_local);
                let mut buf = vec![0u8; READ_BURST];

                let trace = std::env::var("NTTY7_TRACE").is_ok_and(|v| !v.is_empty() && v != "0");
                let mut tr_last = std::time::Instant::now();
                let mut tr_bytes: u64 = 0;
                let mut tr_reads: u32 = 0;
                let mut tr_read_t = std::time::Duration::ZERO;
                let mut tr_disp_t = std::time::Duration::ZERO;
                let mut next_remote_check = std::time::Instant::now();
                let mut next_group_check = std::time::Instant::now();
                let mut last_group = None;
                // Two samples have to agree before a plain burst stops asking.
                // The first read of a pane, and the read after a group change,
                // still see a program that took the tty between those samples.
                let mut group_seen = false;
                let mut group_settled = false;

                loop {
                    if trace && tr_last.elapsed() >= std::time::Duration::from_secs(1) {
                        // Not `eprintln!`: a daemon can be running without any
                        // standard error at all (see the pane reader), and a
                        // failed write there would panic the reader thread.
                        let _ = writeln!(
                            std::io::stderr(),
                            "[trace daemon] {:.1} MB/s | {} reads ({} B/read) | pty wait {:?} dispatch {:?}",
                            tr_bytes as f64 / tr_last.elapsed().as_secs_f64() / 1e6,
                            tr_reads,
                            if tr_reads > 0 { tr_bytes / tr_reads as u64 } else { 0 },
                            tr_read_t,
                            tr_disp_t,
                        );
                        tr_last = std::time::Instant::now();
                        tr_bytes = 0;
                        tr_reads = 0;
                        tr_read_t = std::time::Duration::ZERO;
                        tr_disp_t = std::time::Duration::ZERO;
                    }
                    gate.wait_below_high_water();
                    let tr0 = trace.then(std::time::Instant::now);
                    // Blocked time in `read` only; backpressure above is not an idle gap.
                    let waited = std::time::Instant::now();
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let idle_gap = waited.elapsed();
                            if let Some(tr0) = tr0 {
                                tr_read_t += tr0.elapsed();
                                tr_reads += 1;
                                tr_bytes += n as u64;
                            }
                            let raw = &buf[..n];
                            let (passthrough, frames, mut signals) = scan_chunk(
                                raw,
                                &mut clipboard,
                                &mut graphics,
                                &mut sniffer,
                                &writer,
                            );
                            let bytes: &[u8] = &passthrough;

                            if signals.shell.iter().any(|s| s.at_prompt) && foreground_running() {
                                suppress_relayed_prompt_marks(&mut signals.shell);
                            }

                            // A prompt is the shell reclaiming the tty; probe now, or a quiet pane keeps the old remote.
                            let back_at_prompt = signals.shell.iter().any(|s| s.at_prompt);
                            // Floods ask on a timer. A short read, a pause, or an unsettled group asks immediately.
                            let now = std::time::Instant::now();
                            let check_group = bytes.len() < GROUP_FLOOD_BYTES
                                || idle_gap >= GROUP_IDLE_GAP
                                || signals.boundary
                                || signals.cwd.is_some()
                                || !group_settled
                                || now >= next_group_check;
                            let mut group_changed = false;
                            if check_group {
                                let group = foreground_group();
                                group_changed = group != last_group;
                                group_settled =
                                    group_seen && group == last_group && !signals.command_started;
                                group_seen = true;
                                last_group = group;
                                next_group_check = now + GROUP_CHECK_INTERVAL;
                            }
                            let poll_now = back_at_prompt
                                || group_changed
                                || now >= next_remote_check;
                            if poll_now {
                                next_remote_check = now + REMOTE_CONTEXT_POLL_INTERVAL;
                            }
                            // `Some` means "a probe ran", and the value inside may
                            // still be `None`: that is the probe that clears a
                            // remote context the program has left.
                            let remote = if poll_now && !remote_managed {
                                Some(foreground_remote())
                            } else {
                                None
                            };
                            let agent = poll_now.then(&foreground_agent_fn).flatten();
                            let probed_cwd = poll_now.then(&foreground_cwd_fn).flatten();

                            let tr1 = trace.then(std::time::Instant::now);
                            let mut st = state.lock().unwrap();
                            fan_out_output(&mut st, bytes, frames, &gate);
                            apply_signals(&mut st, signals);
                            if let Some(remote) = remote {
                                apply_remote_context(&mut st, remote);
                            }
                            // Keep kitty file/shm transfer gated on the pane's
                            // *current* locality: an `ssh` that just took the PTY
                            // must stop us honoring host-local object names. Cheap
                            // and only meaningful when a probe follows.
                            graphics.set_local(st.remote.is_none());
                            remote_managed = st
                                .remote
                                .as_ref()
                                .is_some_and(|remote| remote.kind != RemoteKind::Ssh);
                            if let Some(agent) = agent {
                                apply_agent(&mut st, agent);
                            }
                            apply_probed_cwd(&mut st, probed_cwd);
                            if let Some(tr1) = tr1 {
                                tr_disp_t += tr1.elapsed();
                            }
                            drop(st);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }

                death.report(&state, &shutting_down);
            })
            .expect("spawn daemon pane reader thread")
    }

    pub fn gate(&self) -> Arc<OutputGate> {
        self.gate.clone()
    }

    /// Queue `bytes` for the PTY. Returns without waiting on the fd, so a
    /// program that has stopped reading (XOFF, a hung child, a full input
    /// buffer) cannot freeze the thread that called this.
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let agent = self.state.lock().unwrap().agent;
        if interrupts(agent, bytes) {
            arm_interrupt(&self.state);
        }
        self.writer.write(bytes);
    }

    /// Resize the pty and echo `Size` while the state lock is held, so the
    /// subscriber parses old-width bytes before the geometry changes.
    pub fn resize(&self, size: WinSize) {
        resize_state(&mut self.state.lock().unwrap(), size);
        self.resize_pty(size);
    }

    fn resize_pty(&self, size: WinSize) {
        if let Ok(master) = self.backend.master.lock()
            && let Some(master) = master.as_ref()
        {
            let _ = master.resize(pty_size(size));
        }
    }

    pub fn kill(&self) {
        self.hangup();
    }

    /// Drop every attached client. Their writers see the channel close, shut
    /// the socket's read side, and the parked `read` in `stream_pane` returns.
    ///
    /// `DeathReporter::report` does not do this once `shutting_down` is set, so
    /// a kill that only signals the shell leaves `spawn_writer` blocked in
    /// `recv` and the stream task holding this pane's `Arc` until process exit.
    fn disconnect_clients(&self) {
        let mut st = self.state.lock().unwrap();
        st.subscriber = None;
        self.gate.reset();
    }

    fn hangup(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.disconnect_clients();
        #[cfg(unix)]
        Self::signal_group(&self.backend, libc::SIGHUP);
        #[cfg(windows)]
        Self::kill_descendants(&self.backend);
        if let Ok(mut child) = self.backend.child.lock() {
            let _ = child.kill();
        }
        #[cfg(unix)]
        Self::signal_group(&self.backend, libc::SIGKILL);
    }

    #[cfg(windows)]
    fn kill_descendants(pty: &PtyBackend) {
        if let Some(pid) = pty.shell_pid {
            let procs = crate::daemon::winproc::snapshot();
            for target in crate::daemon::winproc::descendants(&procs, pid) {
                crate::daemon::winproc::terminate(target);
            }
        }
    }

    #[cfg(windows)]
    fn spawn_exit_monitor(
        shell_pid: Option<u32>,
        master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        death: Arc<DeathReporter>,
    ) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        let Some(pid) = shell_pid else { return };
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // The process may have exited before OpenProcess ran, so go
            // straight to the close-and-report path — off this thread, since
            // pane construction is what calls us.
            std::thread::Builder::new()
                .name("tty7-daemon-pane-exit-fallback".to_string())
                .spawn(move || {
                    drain_then_report(master, state, shutting_down, death, EXIT_DRAIN_WINDOW);
                })
                .expect("spawn daemon pane exit fallback thread");
            return;
        }
        let handle = handle as isize;
        std::thread::Builder::new()
            .name("tty7-daemon-pane-exit-monitor".to_string())
            .spawn(move || {
                let handle = handle as windows_sys::Win32::Foundation::HANDLE;
                unsafe {
                    WaitForSingleObject(handle, INFINITE);
                    CloseHandle(handle);
                }
                drain_then_report(master, state, shutting_down, death, EXIT_DRAIN_WINDOW);
            })
            .expect("spawn daemon pane exit monitor thread");
    }

    #[cfg(unix)]
    fn signal_group(pty: &PtyBackend, sig: libc::c_int) {
        if let Some(pid) = pty.shell_pid {
            unsafe {
                libc::killpg(pid as libc::pid_t, sig);
            }
        }
        let fg = pty
            .master
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()));
        if let Some(fg) = fg
            && Some(fg as u32) != pty.shell_pid
        {
            unsafe {
                libc::killpg(fg, sig);
            }
        }
    }
}

impl Drop for DaemonPane {
    fn drop(&mut self) {
        self.hangup();
        if let Ok(mut child) = self.backend.child.lock() {
            match child.wait() {
                Ok(status) => log::info!(
                    "pane {} child exited; child_pid={:?} status={status}",
                    self.id,
                    self.backend.shell_pid
                ),
                Err(e) => log::warn!("pane {} child wait failed: {e}", self.id),
            }
        }
        // The child is gone, so a write parked in a full PTY buffer should
        // already have returned. If it has not, shutdown abandons the thread
        // instead of waiting on the fd.
        self.writer.shutdown();
        if let Some(handle) = self.reader.lock().unwrap().take() {
            join_bounded(handle, Duration::from_secs(2));
        }
        if let Some(dir) = self.backend.integration_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        // The pane is over — by a kill, by the shell exiting, or by the daemon
        // shutting down — so whatever it typed goes back to the history file it
        // was seeded from. A handoff does not come through here: nothing is
        // dropped across an `exec`, which is exactly right, because the pane on
        // the other side is the same pane and still owns its file.
        crate::daemon::history::retire(self.id);
    }
}

fn join_bounded(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new()
        .name("tty7-daemon-pane-join".to_string())
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        })
        .is_err()
    {
        return false;
    }
    rx.recv_timeout(timeout).is_ok()
}

fn pty_size(size: WinSize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: size.cols.saturating_mul(size.cell_w),
        pixel_height: size.rows.saturating_mul(size.cell_h),
    }
}

#[cfg(test)]
fn agent_state_snapshot(st: &PaneState) -> Option<PaneAgentState> {
    st.agent_session.clone().map(|state| PaneAgentState {
        pane_id: st.id,
        agent: st.agent,
        state,
    })
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneAgentState {
    pane_id: u64,
    agent: Option<crate::core::cli_agent::CLIAgent>,
    state: crate::core::cli_agent::AgentSessionState,
}

fn apply_signals(st: &mut PaneState, signals: SniffSignals) {
    if let Some(cwd) = signals.cwd
        && st.cwd.as_ref() != Some(&cwd)
    {
        notify(st, DaemonMsg::Cwd(cwd.clone()));
        st.cwd = Some(cwd);
    }
    for shell in signals.shell {
        #[cfg(windows)]
        if shell_mark_capture_changed(&st.shell, &shell) {
            apply_agent(
                st,
                agent_from_shell_mark(&shell, &crate::core::config::agent_detection_aliases()),
            );
        }
        st.shell = shell.clone();
        notify(
            st,
            DaemonMsg::Prompt {
                active: shell.active,
                at_prompt: shell.at_prompt,
            },
        );
    }
    apply_agent_signals(st, signals.agent_events, signals.notification);
}

#[cfg_attr(not(windows), allow(dead_code))]
fn shell_mark_capture_changed(prev: &ShellState, next: &ShellState) -> bool {
    prev.command != next.command
}

#[cfg_attr(not(windows), allow(dead_code))]
fn agent_from_shell_mark(
    shell: &ShellState,
    custom: &std::collections::HashMap<String, String>,
) -> Option<(crate::core::cli_agent::CLIAgent, Vec<String>)> {
    let cmd = shell.command.as_deref()?;
    let agent = crate::core::cli_agent::CLIAgent::detect_from_command_with(cmd, custom)?;
    Some((agent, crate::core::cli_agent::command_argv(cmd)))
}

/// Start waiting out an interrupt key on a pane whose agent is mid-turn.
///
/// The key only *may* have stopped the turn — it also dismisses menus and
/// clears half-typed input — so nothing changes until [`INTERRUPT_SETTLE`]
/// passes with no hook event. A guess that still turns out wrong is undone by
/// the agent's next finished tool ([`AgentSessionState::apply_event`]).
///
/// [`AgentSessionState::apply_event`]: crate::core::cli_agent::AgentSessionState::apply_event
fn arm_interrupt(state: &Arc<Mutex<PaneState>>) {
    let generation = {
        let mut st = state.lock().unwrap();
        let armable = st
            .agent_session
            .as_ref()
            .is_some_and(|s| s.rich && s.mid_turn());
        if !armable || st.agent_clock.interrupt_pending {
            return;
        }
        st.agent_clock.interrupt_pending = true;
        st.agent_clock.generation
    };
    let waiter = state.clone();
    let spawned = std::thread::Builder::new()
        .name("tty7-agent-interrupt".into())
        .spawn(move || {
            std::thread::sleep(INTERRUPT_SETTLE);
            let mut st = waiter.lock().unwrap();
            settle_interrupt(&mut st, generation);
        });
    if spawned.is_err() {
        state.lock().unwrap().agent_clock.interrupt_pending = false;
    }
}

fn settle_interrupt(st: &mut PaneState, armed_at: u64) {
    st.agent_clock.interrupt_pending = false;
    if st.agent_clock.generation != armed_at || !st.alive {
        return;
    }
    let Some(sess) = st.agent_session.as_mut().filter(|s| s.mid_turn()) else {
        return;
    };
    let before = sess.status;
    sess.assume_interrupted();
    sess.note_transition(Some(before), unix_ms());
    notify(st, DaemonMsg::AgentStatus(st.agent_session.clone()));
}

fn apply_agent_signals(
    st: &mut PaneState,
    events: Vec<crate::core::cli_agent::AgentEvent>,
    notification: Option<String>,
) {
    use crate::core::cli_agent::{AgentSessionState, AgentStatus};

    if events.is_empty() && notification.is_none() {
        return;
    }
    let before = st.agent_session.clone();
    let now_ms = unix_ms();

    for event in &events {
        // A hook names the agent that emitted it. Once this pane has a
        // foreground agent, a different one's session id must not land here:
        // omp forking with a Claude UUID is "session not found".
        if let (Some(current), Some(from)) = (st.agent, event.agent)
            && current != from
        {
            continue;
        }
        if st.agent.is_none() && event.agent.is_some() {
            st.agent = event.agent;
            notify(st, DaemonMsg::Agent(st.agent));
        }
        st.agent_clock.generation = st.agent_clock.generation.wrapping_add(1);
        // Stamped per event: a status that moves and comes back within one
        // batch (Done, a new prompt, Done again) began a fresh spell.
        let sess = st
            .agent_session
            .get_or_insert_with(AgentSessionState::default);
        let was = sess.status;
        sess.apply_event(event);
        sess.note_transition(Some(was), now_ms);
    }

    if let Some(body) = notification
        && st.agent.is_some()
        && !st.agent_session.as_ref().is_some_and(|s| s.rich)
    {
        let sess = st
            .agent_session
            .get_or_insert_with(AgentSessionState::default);
        sess.status = AgentStatus::Waiting;
        sess.message = Some(body);
    }

    if let (Some(sess), Some(argv)) = (&mut st.agent_session, &st.agent_argv)
        && sess.launch_argv.is_none()
    {
        sess.launch_argv = Some(argv.clone());
    }

    // The notification-only path, and a session this batch created.
    if let Some(sess) = st.agent_session.as_mut() {
        sess.note_transition(before.as_ref().map(|b| b.status), now_ms);
    }

    if st.agent_session != before {
        notify(st, DaemonMsg::AgentStatus(st.agent_session.clone()));
    }
}

/// Wall-clock milliseconds, for the [`AgentSessionState::since_ms`] stamp a
/// client reads back as an age. Not an `Instant`: the stamp crosses the
/// socket and survives a handoff to the next daemon.
///
/// [`AgentSessionState::since_ms`]: crate::core::cli_agent::AgentSessionState::since_ms
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn apply_probed_cwd(st: &mut PaneState, probed: Option<PathBuf>) {
    let Some(probed) = probed else {
        return;
    };
    if st.remote.is_some() {
        return;
    }
    if st.cwd.as_deref().is_some_and(|cur| same_dir(cur, &probed)) {
        return;
    }
    notify(st, DaemonMsg::Cwd(probed.clone()));
    st.cwd = Some(probed);
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// Clear `at_prompt` on a batch of OSC 133 marks a foreground program is relaying.
///
/// Called when a foreground program owns the pty, which means the marks are
/// being relayed — an `ssh` passing the far shell's prompt through, a nested
/// shell drawing its own. `at_prompt` is what [`DaemonMsg::Prompt`] reports,
/// and the pane treats that as this shell sitting at its own prompt (the
/// prompt cursor, command-duration notifications, coming back off the
/// alternate screen). A relayed prompt must not say that, so the flag is
/// cleared across the whole batch. Marks that then agree on `active`,
/// `at_prompt` and `command` collapse into one `Prompt`.
fn suppress_relayed_prompt_marks(shell: &mut Vec<ShellState>) {
    for s in shell.iter_mut() {
        s.at_prompt = false;
    }
    shell.dedup_by(|a, b| {
        a.active == b.active && a.at_prompt == b.at_prompt && a.command == b.command
    });
}

fn apply_remote_context(st: &mut PaneState, remote: Option<RemoteContext>) {
    if st.remote == remote {
        return;
    }
    st.cwd = None;
    notify(st, DaemonMsg::RemoteContext(remote.clone()));
    st.remote = remote;
}

fn apply_agent(
    st: &mut PaneState,
    detected: Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>,
) {
    let (agent, argv) = match detected {
        Some((agent, argv)) => (Some(agent), Some(argv)),
        None => (None, None),
    };
    if st.agent == agent {
        stamp_launch_argv(st, argv);
        return;
    }
    // The session belongs to whoever was in the foreground. Switching from
    // Claude to omp (or back to the shell) must not keep the previous id.
    if st.agent_session.is_some() {
        st.agent_session = None;
        st.agent_clock.generation = st.agent_clock.generation.wrapping_add(1);
        notify(st, DaemonMsg::AgentStatus(None));
    }
    if agent.is_none() {
        st.agent_argv = None;
    }
    notify(st, DaemonMsg::Agent(agent));
    st.agent = agent;
    stamp_launch_argv(st, argv);
}

fn stamp_launch_argv(st: &mut PaneState, argv: Option<Vec<String>>) {
    let Some(argv) = argv else { return };
    if argv.is_empty() {
        return;
    }
    if st.agent_argv.as_ref() == Some(&argv) {
        return;
    }
    st.agent_argv = Some(argv.clone());
    if let Some(sess) = &mut st.agent_session
        && sess.launch_argv.as_ref() != Some(&argv)
    {
        sess.launch_argv = Some(argv);
        notify(st, DaemonMsg::AgentStatus(st.agent_session.clone()));
    }
}

fn foreground_command_running(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    shell_pid: Option<u32>,
) -> bool {
    is_foreground_command(pty_foreground_pgid(master), shell_pid)
}

#[cfg(unix)]
fn pty_foreground_pgid(master: &Mutex<Option<Box<dyn MasterPty + Send>>>) -> Option<i32> {
    master
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))
}

#[cfg(not(unix))]
fn pty_foreground_pgid(_master: &Mutex<Option<Box<dyn MasterPty + Send>>>) -> Option<i32> {
    None
}

fn is_foreground_command(fg_pgid: Option<i32>, shell_pid: Option<u32>) -> bool {
    match (fg_pgid, shell_pid) {
        (Some(pg), Some(shell)) if pg > 0 => pg as u32 != shell,
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn foreground_cwd(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    shell_pid: Option<u32>,
) -> Option<PathBuf> {
    use std::ffi::CStr;

    let read_cwd = |pid: i32| -> Option<PathBuf> {
        if pid <= 0 {
            return None;
        }
        let mut vinfo: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut vinfo as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret != size {
            return None;
        }
        let s = unsafe { CStr::from_ptr(vinfo.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) }
            .to_str()
            .ok()?;
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    };

    pty_foreground_pgid(master)
        .and_then(read_cwd)
        .or_else(|| read_cwd(shell_pid.map(|p| p as i32).unwrap_or(0)))
}

#[cfg(target_os = "linux")]
fn foreground_cwd(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    shell_pid: Option<u32>,
) -> Option<PathBuf> {
    let read_cwd = |pid: i32| -> Option<PathBuf> {
        if pid <= 0 {
            return None;
        }
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        cwd.is_dir().then_some(cwd)
    };
    pty_foreground_pgid(master)
        .and_then(read_cwd)
        .or_else(|| read_cwd(shell_pid.map(|p| p as i32).unwrap_or(0)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_cwd(
    _master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    _shell_pid: Option<u32>,
) -> Option<PathBuf> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_argv(pid: i32) -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        if pid <= 0 {
            return None;
        }
        let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let argv: Vec<String> = bytes
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        return (!argv.is_empty()).then_some(argv);
    }
    #[cfg(target_os = "macos")]
    {
        if pid <= 0 {
            return None;
        }
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut len = 0usize;
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || len < std::mem::size_of::<libc::c_int>()
        {
            return None;
        }
        let mut buf = vec![0u8; len];
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buf.truncate(len);
        parse_macos_procargs(&buf)
    }
}

#[cfg(target_os = "macos")]
fn parse_macos_procargs(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < std::mem::size_of::<libc::c_int>() {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if i > start {
            argv.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        }
        while i < buf.len() && buf[i] == 0 {
            i += 1;
        }
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_ssh_invocation(argv: &[String]) -> Option<RemoteContext> {
    let program = argv.first()?;
    let name = std::path::Path::new(program).file_name()?.to_string_lossy();
    if name != "ssh" {
        return None;
    }

    let mut i = 1;
    let mut target = None;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            target = Some(arg.clone());
            i += 1;
            break;
        }
        if arg == "-W" || arg == "-w" || arg == "-L" || arg == "-R" || arg == "-D" {
            return None;
        }
        if arg == "-N" || arg == "-f" {
            return None;
        }
        if arg.starts_with("--") {
            return None;
        }
        if let Some(short) = arg.strip_prefix('-')
            && !short.starts_with('-')
            && short.len() > 1
        {
            let mut chars = short.chars();
            let flag = chars.next()?;
            if ssh_option_takes_value(flag) && chars.as_str().is_empty() {
                i += 1;
                if i >= argv.len() {
                    return None;
                }
            }
            i += 1;
            continue;
        }
        if arg.len() == 2 {
            let flag = arg.as_bytes()[1] as char;
            if ssh_option_takes_value(flag) {
                i += 1;
                if i >= argv.len() {
                    return None;
                }
            }
        }
        i += 1;
    }

    let target = target?;
    if i < argv.len() {
        return None;
    }
    Some(RemoteContext {
        kind: RemoteKind::Ssh,
        argv: argv.to_vec(),
        target,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ssh_option_takes_value(flag: char) -> bool {
    matches!(
        flag,
        'B' | 'b'
            | 'c'
            | 'D'
            | 'E'
            | 'e'
            | 'F'
            | 'I'
            | 'i'
            | 'J'
            | 'L'
            | 'l'
            | 'm'
            | 'O'
            | 'o'
            | 'p'
            | 'Q'
            | 'R'
            | 'S'
            | 'W'
            | 'w'
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_remote_context(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<RemoteContext> {
    let pid = master
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))?;
    let argv = foreground_argv(pid)?;
    parse_ssh_invocation(&argv)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_remote_context(
    _master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<RemoteContext> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_agent(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> {
    let detect = || {
        let pid = master
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))?;
        let argv = foreground_argv(pid)?;
        let agent = crate::core::cli_agent::CLIAgent::detect_from_argv_with(
            &argv,
            &crate::core::config::agent_detection_aliases(),
        )?;
        Some((agent, argv))
    };
    Some(detect())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_agent(
    _master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> {
    None
}

#[derive(Default, Clone, PartialEq, Eq)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    command: Option<String>,
}

#[derive(Default)]
struct SniffSignals {
    cwd: Option<PathBuf>,
    shell: Vec<ShellState>,
    agent_events: Vec<crate::core::cli_agent::AgentEvent>,
    notification: Option<String>,
    /// This chunk carried an OSC 133 `B`, `C`, or `D`.
    boundary: bool,
    /// OSC 133 `C`: a command just started. The new process group often
    /// shows up on the next read rather than this one.
    command_started: bool,
}

struct OscSniffer {
    tok: OscTokenizer,
    shell: ShellState,
    notes: crate::core::osc::Notifications,
}

impl OscSniffer {
    fn new() -> Self {
        Self {
            tok: OscTokenizer::new(&[b"7", b"133", b"9", b"99", b"777"]),
            shell: ShellState::default(),
            notes: crate::core::osc::Notifications::default(),
        }
    }

    fn is_ground(&self) -> bool {
        self.tok.is_ground()
    }

    fn feed(&mut self, bytes: &[u8]) -> SniffSignals {
        let mut signals = SniffSignals::default();
        let shell = &mut self.shell;
        let notes = &mut self.notes;
        self.tok.feed(bytes, |payload| {
            if let Some(path) = parse_osc7(payload) {
                signals.cwd = Some(path);
            } else if let Some(rest) = payload.strip_prefix(b"133;") {
                match rest.first() {
                    Some(b'B' | b'D') => signals.boundary = true,
                    Some(b'C') => {
                        signals.boundary = true;
                        signals.command_started = true;
                    }
                    _ => {}
                }
                if handle_osc133(shell, rest) {
                    match signals.shell.last_mut() {
                        Some(last) if last.at_prompt == shell.at_prompt => {
                            *last = shell.clone();
                        }
                        _ => signals.shell.push(shell.clone()),
                    }
                }
            } else if let Some(event) = crate::core::cli_agent::parse_agent_event(payload) {
                signals.agent_events.push(event);
            } else if let Some((title, body)) = notes.parse(payload)
                && title.as_deref() != Some(crate::core::cli_agent::AGENT_EVENT_SENTINEL)
            {
                signals.notification = Some(body);
            }
        });
        signals
    }
}

fn handle_osc133(shell: &mut ShellState, rest: &[u8]) -> bool {
    shell.active = true;
    match rest.first() {
        Some(b'A') | Some(b'B') => shell.at_prompt = true,
        Some(b'C') => {
            shell.at_prompt = false;
            shell.command = rest
                .strip_prefix(b"C;")
                .map(|c| String::from_utf8_lossy(&percent_decode(c)).into_owned())
                .filter(|s| !s.trim().is_empty());
        }
        Some(b'D') => {
            // The optional status (`133;D;130`) is not stored. The mark
            // itself — back at the prompt, command cleared — is the
            // command-finished signal.
            shell.at_prompt = true;
            shell.command = None;
        }
        _ => return false,
    }
    true
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    let s = String::from_utf8_lossy(bytes);
    PathBuf::from(strip_uri_drive_slash(s.as_ref()))
}

#[cfg_attr(unix, allow(dead_code))]
fn strip_uri_drive_slash(path: &str) -> &str {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

pub(crate) fn parse_osc7(payload: &[u8]) -> Option<PathBuf> {
    let rest = payload.strip_prefix(b"7;")?;
    let path_bytes: &[u8] = if let Some(after) = rest.strip_prefix(b"file://") {
        let idx = after.iter().position(|&c| c == b'/')?;
        &after[idx..]
    } else if rest.first() == Some(&b'/') {
        rest
    } else {
        return None;
    };
    let decoded = percent_decode(path_bytes);
    if decoded.is_empty() {
        return None;
    }
    Some(path_from_bytes(&decoded))
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%'
            && i + 2 < input.len()
            && let (Some(h), Some(l)) = (hex_val(input[i + 1]), hex_val(input[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    /// A pane inherits the directory the last one *reported* — the logical
    /// path, `/tmp/x` and not the `/private/tmp/x` the kernel resolves it to.
    /// `cwd()` alone loses that: the shell falls back to `getcwd()` and one
    /// tab reads `/tmp/x` while the tab opened from it reads `/private/tmp/x`.
    /// `PWD` is what carries the name across, and what the shell checks.
    /// An AppImage launch leaves `ARGV0` in the daemon's environment; a zsh
    /// that inherits it renames every command it runs, which breaks Volta's
    /// shims (#1100).
    #[test]
    fn the_appimage_runtimes_argv0_does_not_reach_the_pane() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.env("ARGV0", "./tty7-26.9.5-linux-x86_64.AppImage");
        apply_common_command_setup(&mut cmd, &None, 1, "sh");
        assert_eq!(cmd.get_env("ARGV0"), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_inherited_directory_keeps_the_name_it_was_reached_by() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        // `CommandBuilder` starts from this process's environment, so `PWD`
        // already names wherever the daemon happens to be. The pane's own
        // directory has to win over it.
        let inherited = cmd.get_env("PWD").map(std::ffi::OsStr::to_owned);
        let dir = Some(std::path::PathBuf::from("/tmp"));
        apply_common_command_setup(&mut cmd, &dir, 1, "sh");
        assert_ne!(cmd.get_env("PWD").map(std::ffi::OsStr::to_owned), inherited);
        assert_eq!(
            cmd.get_env("PWD"),
            Some(std::ffi::OsStr::new("/tmp")),
            "the shell is told which name it arrived by"
        );
    }
    use super::*;
    use crate::core::kitty_graphics::ImageDelete;
    use std::path::Path;

    #[test]
    fn a_shell_that_cannot_run_is_named_once_not_wrapped_four_deep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("not-a-shell");
        let problem = shell_program_problem(&missing.to_string_lossy()).expect("a problem");
        assert!(problem.contains("no such shell"), "{problem}");
        assert!(problem.contains("not-a-shell"), "{problem}");
        // One sentence: none of the layers this used to arrive wrapped in.
        assert!(!problem.contains("ENOENT"), "{problem}");
        assert!(!problem.contains("Unable to spawn"), "{problem}");

        // A directory is a different mistake and says so.
        let as_dir = dir.path().to_string_lossy().to_string();
        assert!(
            shell_program_problem(&as_dir)
                .expect("a problem")
                .contains("directory")
        );

        // A bare name goes through PATH; guessing at it here would be worse
        // than saying nothing.
        assert_eq!(shell_program_problem("zsh"), None);

        // Something real passes. The test binary is the one path guaranteed
        // to exist and carry the execute bit on every platform this runs on —
        // `/bin/sh` named a file that is simply absent on Windows, so the
        // check meant to prove a good shell passes proved the opposite there.
        let real = std::env::current_exe().expect("the running test binary");
        assert_eq!(shell_program_problem(&real.to_string_lossy()), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_without_the_execute_bit_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-executable");
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let problem = shell_program_problem(&path.to_string_lossy()).expect("a problem");
        assert!(problem.contains("not executable"), "{problem}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(shell_program_problem(&path.to_string_lossy()), None);
    }

    #[test]
    fn initial_working_directory_skips_paths_that_are_not_directories() {
        let real = std::env::temp_dir();
        assert!(real.is_dir(), "temp dir should exist");

        assert_eq!(
            initial_working_directory(Some(real.clone())),
            Some(real.clone())
        );

        for bogus in [
            "/home/someone/definitely-not-here",
            "/c/Users/definitely-not-here",
        ] {
            let got = initial_working_directory(Some(PathBuf::from(bogus)));
            assert_ne!(
                got.as_deref(),
                Some(Path::new(bogus)),
                "{bogus} is not a directory here and must not be handed to spawn"
            );
            if let Some(d) = got {
                assert!(d.is_dir(), "fallback {d:?} must be a real directory");
            }
        }

        let file = real.join("tty7-iwd-probe");
        std::fs::write(&file, b"x").expect("write probe file");
        let got = initial_working_directory(Some(file.clone()));
        assert_ne!(got.as_deref(), Some(file.as_path()));
        let _ = std::fs::remove_file(&file);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_pane_reports_an_uninstrumented_shells_cwd() {
        let target = std::path::Path::new("/usr");

        let (tx, rx) = output_pipe();
        let pane = DaemonPane::spawn_subscribed(
            1,
            Some(PathBuf::from("/")),
            ws(80, 24),
            Some(ShellSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "cd /usr && exec cat".into()],
                args_are_tty7_defaults: false,
            }),
            true,
            tx,
            || {},
        )
        .expect("spawn pane");

        let mut reported = None;
        for _ in 0..200 {
            pane.write_input(b"\n");
            while let Ok(msg) = rx.try_recv() {
                if let DaemonMsg::Cwd(p) = msg {
                    reported = Some(p);
                }
            }
            if reported.as_deref() == Some(target) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        pane.kill();

        assert_eq!(
            reported.as_deref(),
            Some(target),
            "a pane whose shell emits no OSC 7 must still report where it \
             actually is — this is what a new tab inherits (issue #187)"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_pty_child_argv_detects_the_agent() {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("bash");
        cmd.args(["-c", "exec -a codex cat"]);
        let mut child = pty.slave.spawn_command(cmd).expect("spawn child");
        let master = Mutex::new(Some(pty.master));

        let mut detected = None;
        for _ in 0..200 {
            if let Some(agent) = foreground_agent(&master).flatten() {
                detected = Some(agent);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();

        let (agent, argv) = detected.expect("agent detected from live PTY child");
        assert_eq!(
            agent,
            crate::core::cli_agent::CLIAgent::Codex,
            "a live PTY child with argv[0]=codex must be detected as Codex"
        );
        assert_eq!(
            argv.first().map(String::as_str),
            Some("codex"),
            "the observed argv rides along with the detection"
        );
    }

    #[test]
    fn choose_shell_prefers_override_then_config_then_default() {
        let over = ShellSpec {
            program: "fish".into(),
            args: vec!["-l".into()],
            args_are_tty7_defaults: true,
        };
        let cfg = ("zsh".to_string(), vec!["-i".to_string()]);

        assert_eq!(
            choose_shell(Some(over.clone()), Some(cfg.clone())),
            Some(ChosenShell {
                program: "fish".to_string(),
                args: vec!["-l".to_string()],
                args_are_tty7_defaults: true,
            })
        );
        assert_eq!(
            choose_shell(None, Some(cfg.clone())),
            Some(ChosenShell {
                program: "zsh".to_string(),
                args: vec!["-i".to_string()],
                args_are_tty7_defaults: false,
            })
        );
        assert_eq!(choose_shell(None, None), None);
    }

    #[test]
    fn only_user_authored_args_block_shell_integration() {
        let chosen = |args: Vec<&str>, tty7: bool| ChosenShell {
            program: r"C:\Program Files\Git\bin\bash.exe".to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            args_are_tty7_defaults: tty7,
        };

        assert!(!has_custom_args(Some(&chosen(vec!["-i", "-l"], true))));
        assert!(has_custom_args(Some(&chosen(vec!["-i", "-l"], false))));
        assert!(!has_custom_args(Some(&chosen(vec![], false))));
        assert!(!has_custom_args(Some(&chosen(vec![], true))));
        assert!(!has_custom_args(None));
    }

    #[cfg(windows)]
    #[test]
    fn wsl_panes_are_tagged_as_a_foreign_filesystem() {
        let spec = |program: &str, args: Vec<&str>| ChosenShell {
            program: program.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            args_are_tty7_defaults: true,
        };

        let ctx = wsl_remote_context(Some(&spec(
            "wsl.exe",
            vec!["--distribution", "Ubuntu-24.04", "--cd", "~"],
        )))
        .expect("wsl.exe must be tagged");
        assert_eq!(ctx.kind, RemoteKind::Wsl);
        assert_eq!(ctx.target, "Ubuntu-24.04");
        assert!(ctx.argv.is_empty());

        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec!["-d", "Debian"])))
                .expect("short flag")
                .target,
            "Debian"
        );
        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec![])))
                .expect("default distro is still WSL")
                .target,
            crate::core::shells::default_wsl_distro().unwrap_or_default(),
            "no --distribution resolves to the machine's default distro"
        );
        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec!["--distribution=Arch"])))
                .expect("joined flag")
                .target,
            "Arch"
        );
        assert!(wsl_remote_context(Some(&spec(r"C:\Windows\System32\WSL.EXE", vec![]))).is_some());

        let from_config = choose_shell(None, Some(("wsl.exe".to_string(), Vec::new())));
        assert_eq!(
            wsl_remote_context(from_config.as_ref()).map(|c| c.kind),
            Some(RemoteKind::Wsl),
            "a configured wsl.exe is as much a WSL pane as a dropdown one"
        );

        assert!(wsl_remote_context(Some(&spec("powershell.exe", vec![]))).is_none());
        assert!(
            wsl_remote_context(Some(&spec(r"C:\Program Files\Git\bin\bash.exe", vec![]))).is_none()
        );
        assert!(wsl_remote_context(None).is_none());
    }

    #[test]
    fn arg_based_integration_rebuilds_default_shell_builder() {
        let mut cmd = CommandBuilder::new_default_prog();
        let injection = shell_integration::Injection {
            env: std::collections::HashMap::new(),
            args: vec!["-C".to_string(), "echo ready".to_string()],
            replaces_argv: false,
            dir: None,
        };

        apply_shell_integration(&mut cmd, "/bin/fish", &injection);

        assert!(!cmd.is_default_prog(), "argv can now be appended safely");
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec!["/bin/fish", "-C", "echo ready"]);
    }

    #[test]
    fn a_configured_zdotdir_feeds_the_redirector_instead_of_replacing_it() {
        let env = vec![
            ("ZDOTDIR".to_string(), "/home/a/.config/zsh".to_string()),
            ("EDITOR".to_string(), "vi".to_string()),
        ];

        let rerouted = reroute_configured_zdotdir(env.clone(), true);
        assert_eq!(
            rerouted,
            vec![
                (
                    "NTTY7_USER_ZDOTDIR".to_string(),
                    "/home/a/.config/zsh".to_string()
                ),
                ("EDITOR".to_string(), "vi".to_string()),
            ]
        );
        // Without the redirector there is nothing to protect.
        assert_eq!(reroute_configured_zdotdir(env.clone(), false), env);
    }

    #[test]
    fn env_only_integration_keeps_default_login_shell_builder() {
        let mut cmd = CommandBuilder::new_default_prog();
        let mut env = std::collections::HashMap::new();
        env.insert("ZDOTDIR".to_string(), "/tmp/tty7-zdotdir-test".to_string());
        let injection = shell_integration::Injection {
            env,
            args: Vec::new(),
            replaces_argv: false,
            dir: None,
        };

        apply_shell_integration(&mut cmd, "/bin/zsh", &injection);

        assert!(
            cmd.is_default_prog(),
            "zsh still launches as the login shell"
        );
        assert_eq!(
            cmd.get_env("ZDOTDIR").and_then(|value| value.to_str()),
            Some("/tmp/tty7-zdotdir-test")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn detected_shell_override_uses_explicit_command_builder() {
        let portable_shell = CommandBuilder::new_default_prog().get_shell();
        let detected_shell = format!("{portable_shell}-detected");
        let cmd = default_prog_with_override(Some(detected_shell.clone()));

        assert!(!cmd.is_default_prog());
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec![detected_shell.clone()]);
        assert_eq!(default_shell_name(&cmd), detected_shell);
    }

    #[cfg(not(windows))]
    #[test]
    fn no_detected_shell_keeps_portable_login_default() {
        let cmd = default_prog_with_override(None);

        assert!(cmd.is_default_prog());
        assert!(!default_shell_name(&cmd).is_empty());
    }

    #[test]
    fn join_bounded_returns_true_when_the_thread_finishes() {
        let handle = std::thread::spawn(|| {});
        assert!(join_bounded(handle, Duration::from_secs(5)));
    }

    #[test]
    fn join_bounded_times_out_on_a_stuck_thread() {
        let (unblock, blocked) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = blocked.recv();
        });
        assert!(!join_bounded(handle, Duration::from_millis(50)));
        drop(unblock);
    }

    #[test]
    fn gate_passes_below_high_water() {
        let gate = OutputGate::new();
        gate.add((OutputGate::HIGH_WATER - 1) as usize);
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "no wait expected"
        );
    }

    #[test]
    fn gate_parks_at_high_water_until_drained() {
        let gate = Arc::new(OutputGate::new());
        gate.add(OutputGate::HIGH_WATER as usize);

        let drainer = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                gate.sub(OutputGate::HIGH_WATER as usize);
            })
        };
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        let waited = t0.elapsed();
        drainer.join().unwrap();
        assert!(waited >= Duration::from_millis(40), "must park until sub()");
        assert!(
            waited < OutputGate::MAX_WAIT,
            "the drain, not the escape timeout, must unpark"
        );
    }

    #[test]
    fn gate_reset_unparks_a_throttled_reader() {
        let gate = Arc::new(OutputGate::new());
        gate.add(OutputGate::HIGH_WATER as usize * 2);

        let resetter = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                gate.reset();
            })
        };
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        resetter.join().unwrap();
        assert!(t0.elapsed() < OutputGate::MAX_WAIT);
    }

    #[test]
    fn gate_tolerates_negative_drift() {
        let gate = OutputGate::new();
        gate.sub(1024);
        gate.add(512);
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        assert!(t0.elapsed() < Duration::from_millis(100));
    }

    fn ws(cols: u16, rows: u16) -> WinSize {
        WinSize {
            cols,
            rows,
            cell_w: 8,
            cell_h: 16,
        }
    }

    /// Issue #857: the two sniffers that rewrite the stream before the ring
    /// and the client see it must hand every byte that is not theirs on
    /// unchanged, wherever a read splits it — a Pi frame (synchronized update,
    /// line marks, CJK text, cursor-up rewrite) at every split point, and a
    /// byte at a time. Both what the ring records and what the client is sent.
    #[test]
    fn out_of_band_stripping_passes_a_streamed_frame_through_every_split() {
        let frame = "\x1b[?2026h\x1b[2A\r\x1b[2K## 资源\r\n\x1b[2K\
                     \x1b]133;B\x07\x1b]133;C\x07下一步。\x1b[?2026l"
            .as_bytes();
        let writer = PtyWriter::spawn(Box::new(std::io::sink()));
        let run = |chunks: &[&[u8]]| {
            let mut clipboard = ClipboardSniffer::default();
            let mut graphics = GraphicsSniffer::new_local(true);
            let (mut recorded, mut sent) = (Vec::new(), Vec::new());
            for chunk in chunks {
                let mut frames = Vec::new();
                let pass =
                    lift_out_of_band(chunk, &mut clipboard, &mut graphics, &writer, &mut frames);
                recorded.extend_from_slice(&pass);
                if frames.is_empty() {
                    sent.extend_from_slice(&pass);
                }
                for f in frames {
                    match f {
                        GraphicsFrame::Output(b) => sent.extend(b),
                        _ => panic!("no out-of-band frame in a plain stream"),
                    }
                }
            }
            (recorded, sent)
        };
        for cut in 0..=frame.len() {
            let (recorded, sent) = run(&[&frame[..cut], &frame[cut..]]);
            assert_eq!(recorded, frame, "ring, split at byte {cut}");
            assert_eq!(sent, frame, "client, split at byte {cut}");
        }
        let bytewise: Vec<&[u8]> = frame.chunks(1).collect();
        assert_eq!(run(&bytewise), (frame.to_vec(), frame.to_vec()));
    }

    #[test]
    fn sniff_osc7_cwd() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/Users/me/dev\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/Users/me/dev")));
    }

    /// An OSC 0 the sniffer does not track must not eat the cwd or the prompt
    /// mark that follows it in the same read.
    #[test]
    fn an_untracked_title_does_not_swallow_the_other_marks_in_the_same_read() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]0;user@host:~/dev\x07\x1b]7;file://host/dev\x07\x1b]133;A\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/dev")));
        assert!(sig.shell.last().unwrap().at_prompt);
    }

    #[test]
    fn sniff_osc133_prompt() {
        let mut s = OscSniffer::new();
        let b = s.feed(b"\x1b]133;B\x07");
        assert!(b.shell.last().unwrap().active);
        assert!(b.shell.last().unwrap().at_prompt);

        let c = s.feed(b"\x1b]133;C;sleep%201\x07");
        assert!(!c.shell.last().unwrap().at_prompt);
        assert_eq!(
            c.shell.last().unwrap().command.as_deref(),
            Some("sleep 1"),
            "the C mark is the command that is running"
        );

        let d = s.feed(b"\x1b]133;D;130\x07");
        assert!(d.shell.last().unwrap().at_prompt);
        assert_eq!(
            d.shell.last().unwrap().command,
            None,
            "the D mark clears the command it finishes"
        );
    }

    #[test]
    fn a_full_command_cycle_in_one_chunk_still_reports_leaving_the_prompt() {
        let mut s = OscSniffer::new();
        s.feed(b"\x1b]133;A\x07\x1b]133;B\x07");

        let sig =
            s.feed(b"\x1b]133;C;echo%20hi\x07hi\r\n\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
        let states: Vec<bool> = sig.shell.iter().map(|s| s.at_prompt).collect();
        assert_eq!(
            states,
            vec![false, true],
            "the chunk must report leaving the prompt and coming back, not just the end state"
        );
        assert_eq!(sig.shell.last().unwrap().command, None);
    }

    #[test]
    fn marks_on_the_same_side_of_the_prompt_boundary_fold_into_one_state() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;D;3\x07\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(sig.shell.len(), 1, "D/A/B is one at-prompt state");
        assert!(sig.shell[0].at_prompt);
        assert_eq!(sig.shell[0].command, None);
    }

    #[test]
    fn sniff_osc133_command_capture_drives_agent_detection() {
        let custom = std::collections::HashMap::new();
        let mut s = OscSniffer::new();

        let c = s.feed(b"\x1b]133;C;claude%20--help\x07");
        let shell = c.shell.last().unwrap();
        assert!(!shell.at_prompt);
        assert_eq!(shell.command.as_deref(), Some("claude --help"));
        assert_eq!(
            agent_from_shell_mark(shell, &custom),
            Some((
                crate::core::cli_agent::CLIAgent::Claude,
                vec!["claude".to_string(), "--help".to_string()],
            ))
        );

        let d = s.feed(b"\x1b]133;D;0\x07");
        let shell = d.shell.last().unwrap();
        assert_eq!(shell.command, None);
        assert_eq!(agent_from_shell_mark(shell, &custom), None);

        let c = s.feed(b"\x1b]133;C;git%20status\x07");
        let shell = c.shell.last().unwrap();
        assert_eq!(shell.command.as_deref(), Some("git status"));
        assert_eq!(agent_from_shell_mark(shell, &custom), None);

        let c = s.feed(b"\x1b]133;C\x07");
        assert_eq!(c.shell.last().unwrap().command, None);

        let _ = s.feed(b"\x1b]133;C;codex\x07");
        let a = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert_eq!(a.shell.last().unwrap().command.as_deref(), Some("codex"));
        let d = s.feed(b"\x1b]133;D;0\x07");
        assert_eq!(d.shell.last().unwrap().command, None);

        let c = s.feed(b"\x1b]133;C;echo%20a%0Aecho%20b\x07");
        assert_eq!(
            c.shell.last().unwrap().command.as_deref(),
            Some("echo a\necho b")
        );

        let c = s.feed(b"\x1b]133;C;%20%20\x07");
        assert_eq!(c.shell.last().unwrap().command, None);
    }

    #[test]
    fn shell_mark_agent_detection_keeps_launch_argv_for_resume() {
        let custom = std::collections::HashMap::new();
        let mut s = OscSniffer::new();

        let c = s.feed(b"\x1b]133;C;claude%20--dangerously-skip-permissions\x07");
        let (agent, argv) = agent_from_shell_mark(c.shell.last().unwrap(), &custom).unwrap();
        assert_eq!(agent, crate::core::cli_agent::CLIAgent::Claude);
        assert_eq!(
            agent.resume_command("abc-123", Some(&argv)).as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc-123")
        );

        // Case must survive capture: a lowercased path or model name would
        // replay the wrong flags.
        let c = s.feed(b"\x1b]133;C;claude%20--model%20Opus\x07");
        let (agent, argv) = agent_from_shell_mark(c.shell.last().unwrap(), &custom).unwrap();
        assert_eq!(argv, ["claude", "--model", "Opus"]);
        assert_eq!(
            agent.resume_command("abc", Some(&argv)).as_deref(),
            Some("claude --model Opus --resume abc")
        );

        // PowerShell call-operator launches keep working end to end.
        let c = s.feed(b"\x1b]133;C;%26%20%22C%3A%5Ctools%5Cclaude.exe%22%20--continue\x07");
        let (agent, argv) = agent_from_shell_mark(c.shell.last().unwrap(), &custom).unwrap();
        assert_eq!(agent, crate::core::cli_agent::CLIAgent::Claude);
        assert_eq!(argv, [r"C:\tools\claude.exe", "--continue"]);
        assert_eq!(
            agent.resume_command("abc", Some(&argv)).as_deref(),
            Some("claude --resume abc"),
            "stale session flags are dropped, not replayed"
        );
    }

    #[test]
    fn sniff_osc133_stray_marks_do_not_reapply_mark_detection() {
        let mut s = OscSniffer::new();

        let mut prev = ShellState::default();
        let c = s.feed(b"\x1b]133;C;.%5Cdev.ps1\x07").shell.pop().unwrap();
        assert!(shell_mark_capture_changed(&prev, &c));
        assert_eq!(
            agent_from_shell_mark(&c, &std::collections::HashMap::new()),
            None
        );
        prev = c;

        let ab = s.feed(b"\x1b]133;A\x1b]133;B\x07").shell.pop().unwrap();
        assert!(!shell_mark_capture_changed(&prev, &ab));
        prev = ab;

        let d = s.feed(b"\x1b]133;D;0\x07").shell.pop().unwrap();
        assert!(shell_mark_capture_changed(&prev, &d));
    }

    #[test]
    fn sniff_osc133_edit_mode_does_not_emit_prompt_state() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;V;1\x07");
        assert!(
            sig.shell.is_empty(),
            "edit-mode metadata must not bump prompt state or prompt sequence"
        );

        let b = s.feed(b"\x1b]133;B\x07");
        assert!(b.shell.last().unwrap().active);
        assert!(b.shell.last().unwrap().at_prompt);
    }

    #[test]
    fn foreground_command_distinguishes_the_shell_from_a_command() {
        assert!(!is_foreground_command(Some(1000), Some(1000)));
        assert!(is_foreground_command(Some(2000), Some(1000)));
        assert!(!is_foreground_command(None, Some(1000)));
        assert!(!is_foreground_command(Some(2000), None));
        assert!(!is_foreground_command(Some(0), Some(1000)));
    }

    #[test]
    fn foreground_program_prompt_marks_do_not_claim_the_prompt() {
        let mut s = OscSniffer::new();
        let mut signals = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            signals.shell.last().unwrap().at_prompt,
            "the raw marks read as at-prompt"
        );

        let ssh_running = is_foreground_command(Some(2000), Some(1000));
        if signals.shell.iter().any(|st| st.at_prompt) && ssh_running {
            suppress_relayed_prompt_marks(&mut signals.shell);
        }
        assert!(
            !signals.shell.last().unwrap().at_prompt,
            "a foreground program's prompt marks must not report this shell at its prompt"
        );

        let mut local = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        let shell_idle = is_foreground_command(Some(1000), Some(1000));
        if local.shell.iter().any(|st| st.at_prompt) && shell_idle {
            suppress_relayed_prompt_marks(&mut local.shell);
        }
        assert!(local.shell.last().unwrap().at_prompt);
    }

    /// A relayed batch still collapses into one `Prompt`. A whole turn can
    /// arrive in one read — `D`, the prompt's `A`/`B`, then `C` — and once
    /// `at_prompt` is cleared those entries agree on the fields the message
    /// actually carries.
    #[test]
    fn suppression_collapses_a_relayed_batch_into_one_prompt() {
        let mut s = OscSniffer::new();
        let mut turn = s.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        suppress_relayed_prompt_marks(&mut turn.shell);
        assert_eq!(turn.shell.len(), 1);
        assert!(!turn.shell.last().unwrap().at_prompt);

        let mut turn = s.feed(b"\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07");
        suppress_relayed_prompt_marks(&mut turn.shell);
        assert_eq!(turn.shell.len(), 1);
        assert!(!turn.shell.last().unwrap().at_prompt);
    }

    #[test]
    fn sniff_resyncs_on_new_osc_after_an_unterminated_one() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            sig.shell.last().is_some_and(|sh| sh.at_prompt),
            "OSC 133;B after an unterminated 133;A was dropped (no resync on `]`)"
        );

        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/dropped\x1b]7;file://host/kept\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/kept")));
    }

    #[test]
    fn at_prompt_covers_prompt_draw_gap_across_chunks() {
        let mut s = OscSniffer::new();

        assert!(!s.feed(b"\x1b]133;C\x07").shell.last().unwrap().at_prompt);

        let d = s.feed(b"\x1b]133;D;0\x07");
        assert!(
            d.shell.last().unwrap().at_prompt,
            "D should mark us back at the prompt before the prompt text is drawn"
        );

        let chunk = s.feed(
            b"\x1b]133;A\x07\x1b]7;file://host/repo/tty7\x07\r\ntty7 git:(main) \xe2\x9e\x9c ",
        );
        assert!(
            chunk.shell.last().unwrap().at_prompt,
            "prompt visible but at_prompt=false — the mis-routing window is still open"
        );

        assert!(s.feed(b"\x1b]133;B\x07").shell.last().unwrap().at_prompt);
    }

    #[test]
    fn pty_size_clamps_and_computes_pixels() {
        let ps = pty_size(WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        });
        assert_eq!(ps.rows, 24);
        assert_eq!(ps.cols, 80);
        assert_eq!(ps.pixel_width, 80 * 8);
        assert_eq!(ps.pixel_height, 24 * 17);

        let z = pty_size(WinSize {
            cols: 0,
            rows: 0,
            cell_w: 0,
            cell_h: 0,
        });
        assert_eq!(z.rows, 1);
        assert_eq!(z.cols, 1);
        assert_eq!(z.pixel_width, 0);
        assert_eq!(z.pixel_height, 0);

        let big = pty_size(WinSize {
            cols: u16::MAX,
            rows: u16::MAX,
            cell_w: u16::MAX,
            cell_h: u16::MAX,
        });
        assert_eq!(big.pixel_width, u16::MAX);
        assert_eq!(big.pixel_height, u16::MAX);
    }

    #[test]
    fn parse_osc7_forms_and_rejections() {
        assert_eq!(
            parse_osc7(b"7;file://host/Users/me/dev"),
            Some(PathBuf::from("/Users/me/dev"))
        );
        assert_eq!(parse_osc7(b"7;file:///etc"), Some(PathBuf::from("/etc")));
        assert_eq!(parse_osc7(b"7;/var/log"), Some(PathBuf::from("/var/log")));
        assert_eq!(
            parse_osc7(b"7;file://host/a%20b"),
            Some(PathBuf::from("/a b"))
        );
        assert_eq!(
            parse_osc7(b"7;file://host/%E4%B8%AD%E6%96%87"),
            Some(PathBuf::from("/中文"))
        );
        assert_eq!(
            parse_osc7(b"7;file://host/tmp/a%2520b"),
            Some(PathBuf::from("/tmp/a%20b"))
        );
        assert!(parse_osc7(b"8;file://host/x").is_none());
        assert!(parse_osc7(b"7;file://host").is_none());
        assert!(parse_osc7(b"7;relative/path").is_none());
        assert!(parse_osc7(b"7;file://host").is_none());
    }

    #[test]
    fn strip_uri_drive_slash_only_unwraps_drive_paths() {
        assert_eq!(strip_uri_drive_slash("/C:/Users/foo"), "C:/Users/foo");
        assert_eq!(strip_uri_drive_slash("/d:/x"), "d:/x");
        assert_eq!(strip_uri_drive_slash("/home/me/dev"), "/home/me/dev");
        assert_eq!(strip_uri_drive_slash("//host/share"), "//host/share");
        assert_eq!(strip_uri_drive_slash("C:/already"), "C:/already");
        assert_eq!(strip_uri_drive_slash("/"), "/");
    }

    #[test]
    fn percent_decode_handles_escapes_and_garbage() {
        assert_eq!(percent_decode(b"a%20b"), b"a b");
        assert_eq!(percent_decode(b"%2F"), b"/");
        assert_eq!(percent_decode(b"%2f"), b"/");
        assert_eq!(percent_decode(b"%GG"), b"%GG");
        assert_eq!(percent_decode(b"x%2"), b"x%2");
        assert_eq!(percent_decode(b"plain"), b"plain");
    }

    #[test]
    fn hex_val_ranges() {
        assert_eq!(hex_val(b'0'), Some(0));
        assert_eq!(hex_val(b'9'), Some(9));
        assert_eq!(hex_val(b'a'), Some(10));
        assert_eq!(hex_val(b'f'), Some(15));
        assert_eq!(hex_val(b'A'), Some(10));
        assert_eq!(hex_val(b'F'), Some(15));
        assert!(hex_val(b'g').is_none());
        assert!(hex_val(b' ').is_none());
        assert!(hex_val(b'/').is_none());
    }

    /// `133;D` is "the command finished", with or without a status token.
    /// The status itself is not kept.
    #[test]
    fn a_d_mark_returns_to_the_prompt_with_or_without_a_status() {
        let mut s = OscSniffer::new();
        for payload in [
            &b"\x1b]133;D\x07"[..],
            &b"\x1b]133;D;oops\x07"[..],
            &b"\x1b]133;D;-1\x07"[..],
        ] {
            s.feed(b"\x1b]133;C;ls\x07");
            let d = s.feed(payload);
            let shell = d.shell.last().unwrap();
            assert!(shell.at_prompt, "{}", String::from_utf8_lossy(payload));
            assert_eq!(shell.command, None);
        }
    }

    /// A discard writer for `spawn_reader` tests that don't inspect the PTY
    /// write-back path (graphics query replies).
    fn null_writer() -> Arc<PtyWriter> {
        PtyWriter::spawn(Box::new(std::io::sink()))
    }

    /// Keystrokes are queued. A PTY that has stopped reading blocks the writer
    /// thread inside `write`, and the call that queued the next chunk returns
    /// without waiting for that write.
    #[test]
    fn queued_pty_writes_return_while_the_pty_is_not_reading() {
        struct Gate {
            started: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
            written: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Gate {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                    let _ = self.release.recv_timeout(Duration::from_secs(5));
                }
                self.written.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = PtyWriter::spawn(Box::new(Gate {
            started: Some(started_tx),
            release: release_rx,
            written: written.clone(),
        }));
        // Dropped before `writer`, including on panic, so a blocked `write`
        // unblocks and the writer thread can be joined.
        let _release = release_tx;

        writer.write(b"stall");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the writer thread reaches the stalled pty");
        let started = std::time::Instant::now();
        writer.write(b"more");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "queueing input must not wait on a pty that is not reading"
        );
        drop(_release);
        assert!(
            writer.wait_idle(Duration::from_secs(2)),
            "once the pty reads again, the queued bytes are written"
        );
        assert_eq!(written.lock().unwrap().as_slice(), b"stallmore");
    }

    /// A query reply sitting in `write_all` must not make a later paste, or
    /// the paste's closing bracket written as its own chunk, disappear.
    #[test]
    fn a_paste_is_not_dropped_while_a_reply_is_in_flight() {
        struct Gate {
            started: Option<std::sync::mpsc::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
            written: Arc<Mutex<Vec<u8>>>,
        }
        impl std::io::Write for Gate {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                    let _ = self.release.recv();
                }
                self.written.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = PtyWriter::spawn(Box::new(Gate {
            started: Some(started_tx),
            release: release_rx,
            written: written.clone(),
        }));
        let _release = release_tx;

        let paste = vec![b'x'; 5 * 1024 * 1024];
        writer.write(b"q");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the writer thread reaches the stalled pty");
        writer.write(&paste);
        writer.write(b"\x1b[201~");
        drop(_release);
        assert!(
            writer.wait_idle(Duration::from_secs(2)),
            "the queued paste has to be written, not dropped"
        );
        let got = written.lock().unwrap();
        assert_eq!(&got[..1], b"q");
        assert_eq!(&got[1..paste.len() + 1], paste.as_slice());
        assert_eq!(&got[paste.len() + 1..], b"\x1b[201~");
    }

    fn test_state(alive: bool) -> PaneState {
        PaneState {
            id: 0,
            size: ws(80, 24),
            subscriber: None,
            cwd: None,
            shell: ShellState::default(),
            remote: None,
            agent: None,
            agent_session: None,
            agent_argv: None,
            agent_clock: AgentClock::default(),
            alive,
        }
    }

    /// The subscriber is installed before the reader, so it hears the pane's
    /// WSL context and then the directory it was born in. The client clears
    /// its cwd on `RemoteContext`; the other order would drop the path.
    #[test]
    fn a_new_subscriber_hears_the_wsl_context_before_the_initial_cwd() {
        let mut st = test_state(true);
        st.remote = Some(RemoteContext {
            kind: RemoteKind::Wsl,
            argv: Vec::new(),
            target: "Ubuntu".into(),
        });
        st.cwd = Some(PathBuf::from("/home/me"));
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);
        publish_standing(&st);
        let msgs: Vec<_> = rx.try_iter().collect();
        assert!(
            matches!(
                msgs.first(),
                Some(DaemonMsg::RemoteContext(Some(ctx)))
                    if ctx.kind == RemoteKind::Wsl && ctx.target == "Ubuntu"
            ),
            "the standing remote context is first, got {msgs:?}"
        );
        assert!(
            matches!(msgs.get(1), Some(DaemonMsg::Cwd(path)) if path == Path::new("/home/me")),
            "the initial directory follows the context, got {msgs:?}"
        );
        assert_eq!(msgs.len(), 2);
    }

    /// Install the one subscriber a pane has. There is no replay to drain.
    fn subscribe(st: &mut PaneState, tx: OutputTx) {
        st.subscriber = Some(tx);
    }

    #[test]
    fn interrupt_keys_are_recognised_in_every_encoding() {
        for key in [
            &b"\x1b"[..],
            b"\x03",
            b"\x1b[27u",
            b"\x1b[27;1u",
            b"\x1b[27;1:1u",
            b"\x1b[27;65u",
            b"\x1b[99;5u",
            b"\x1b[99;5:1u",
            b"\x1b[99:67;5u",
            b"\x1b[27;5;99~",
        ] {
            assert!(is_interrupt_key(key), "{key:?}");
        }
        for key in [
            &b"\x1b[A"[..],
            b"\x1b\x1b",
            b"\x1b[27;1:3u",
            b"\x1b[27;3u",
            b"\x1b[99u",
            b"\x1b[99;6u",
            b"abc\x03",
            b"\x1b[13u",
            b"q",
        ] {
            assert!(!is_interrupt_key(key), "{key:?}");
        }
    }

    /// Grok Build's Esc does not stop its turn; its Ctrl+C does.
    #[test]
    fn grok_builds_turn_is_stopped_by_ctrl_c_alone() {
        use crate::core::cli_agent::CLIAgent;
        let grok = Some(CLIAgent::Grok);
        assert!(!interrupts(grok, b"\x1b"));
        assert!(!interrupts(grok, b"\x1b[27;1:1u"));
        assert!(interrupts(grok, b"\x03"));
        assert!(interrupts(grok, b"\x1b[99;5u"));
        assert!(interrupts(Some(CLIAgent::Pi), b"\x1b"));
        assert!(interrupts(None, b"\x1b"));
    }

    fn working_session() -> crate::core::cli_agent::AgentSessionState {
        crate::core::cli_agent::AgentSessionState {
            status: crate::core::cli_agent::AgentStatus::Working,
            rich: true,
            ..Default::default()
        }
    }

    #[test]
    fn an_interrupt_settles_only_if_nothing_spoke_after_it() {
        use crate::core::cli_agent::AgentStatus;

        let mut st = test_state(true);
        st.agent_session = Some(working_session());
        st.agent_clock.interrupt_pending = true;
        let armed = st.agent_clock.generation;
        settle_interrupt(&mut st, armed);
        let sess = st.agent_session.as_ref().unwrap();
        assert_eq!(sess.status, AgentStatus::Done);
        assert!(sess.inferred);
        assert!(!st.agent_clock.interrupt_pending);

        let mut st = test_state(true);
        st.agent_session = Some(working_session());
        let armed = st.agent_clock.generation;
        st.agent_clock.generation += 1;
        settle_interrupt(&mut st, armed);
        assert_eq!(
            st.agent_session.as_ref().unwrap().status,
            AgentStatus::Working,
            "a hook event after the key has the last word"
        );

        let mut st = test_state(false);
        st.agent_session = Some(working_session());
        let armed = st.agent_clock.generation;
        settle_interrupt(&mut st, armed);
        assert_eq!(
            st.agent_session.as_ref().unwrap().status,
            AgentStatus::Working,
            "a dead pane is left to its exit report"
        );
    }

    #[test]
    fn an_interrupt_arms_once_and_only_mid_turn() {
        let state = Arc::new(Mutex::new(test_state(true)));
        arm_interrupt(&state);
        assert!(
            !state.lock().unwrap().agent_clock.interrupt_pending,
            "no agent"
        );

        state.lock().unwrap().agent_session = Some(crate::core::cli_agent::AgentSessionState {
            rich: false,
            ..working_session()
        });
        arm_interrupt(&state);
        assert!(
            !state.lock().unwrap().agent_clock.interrupt_pending,
            "a status without hooks behind it has no Stop to miss"
        );

        state.lock().unwrap().agent_session = Some(working_session());
        arm_interrupt(&state);
        assert!(state.lock().unwrap().agent_clock.interrupt_pending);
    }

    #[test]
    fn hook_events_restart_the_agent_clock() {
        use crate::core::cli_agent::{AgentEvent, AgentEventKind};

        let mut st = test_state(true);
        let before = st.agent_clock.generation;
        apply_agent_signals(
            &mut st,
            vec![AgentEvent {
                agent: None,
                kind: AgentEventKind::PromptSubmit,
                session_id: None,
                message: None,
                cwd: None,
                source: None,
                readout: Default::default(),
            }],
            None,
        );
        assert_ne!(st.agent_clock.generation, before);
    }

    #[test]
    fn a_status_that_leaves_and_returns_in_one_batch_is_restamped() {
        use crate::core::cli_agent::{AgentEvent, AgentEventKind, AgentSessionState, AgentStatus};

        let event = |kind| AgentEvent {
            agent: None,
            kind,
            session_id: None,
            message: None,
            cwd: None,
            source: None,
            readout: Default::default(),
        };
        let mut st = test_state(true);
        st.agent_session = Some(AgentSessionState {
            status: AgentStatus::Done,
            since_ms: Some(1),
            ..Default::default()
        });
        apply_agent_signals(
            &mut st,
            vec![
                event(AgentEventKind::PromptSubmit),
                event(AgentEventKind::Stop),
            ],
            None,
        );
        let sess = st.agent_session.as_ref().unwrap();
        assert_eq!(sess.status, AgentStatus::Done);
        assert!(
            sess.since_ms.is_some_and(|ms| ms > 1),
            "a new turn finished, so its Done is not the old one"
        );
    }

    #[test]
    fn sentinel_events_drive_agent_session_state() {
        use crate::core::cli_agent::{AgentStatus, CLIAgent};

        let mut st = test_state(true);
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);

        let mut sniffer = OscSniffer::new();
        let stream = concat!(
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"v":1,"agent":"claude","event":"session-start","session_id":"sid-9"}"#,
            "\x07",
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"v":1,"agent":"claude","event":"prompt-submit"}"#,
            "\x07",
        );
        apply_signals(&mut st, sniffer.feed(stream.as_bytes()));

        assert_eq!(st.agent, Some(CLIAgent::Claude));
        let sess = st.agent_session.clone().expect("session state exists");
        assert_eq!(sess.status, AgentStatus::Working);
        assert_eq!(sess.session_id.as_deref(), Some("sid-9"));
        assert!(sess.rich);

        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Agent(Some(CLIAgent::Claude)))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::AgentStatus(Some(s))) if s.status == AgentStatus::Working
        ));

        let waiting = concat!(
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"event":"notification","message":"Claude needs your permission to use Bash"}"#,
            "\x07",
        );
        apply_signals(&mut st, sniffer.feed(waiting.as_bytes()));
        assert_eq!(
            st.agent_session.as_ref().unwrap().status,
            AgentStatus::Waiting
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::AgentStatus(Some(s))) if s.message.as_deref().unwrap().contains("permission")
        ));

        apply_agent(&mut st, None);
        assert!(st.agent_session.is_none());
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::AgentStatus(None))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Agent(None))));
    }

    #[test]
    fn a_foreign_agent_hook_does_not_replace_the_foreground_session() {
        use crate::core::cli_agent::CLIAgent;

        let mut st = test_state(true);
        let mut sniffer = OscSniffer::new();
        let omp = concat!(
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"v":1,"agent":"omp","event":"session-start","session_id":"01a0d21f-2f79-72e0-bb16-d6d908aaa6e0"}"#,
            "\x07",
        );
        apply_signals(&mut st, sniffer.feed(omp.as_bytes()));
        assert_eq!(st.agent, Some(CLIAgent::OhMyPi));

        let claude = concat!(
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"v":1,"agent":"claude","event":"session-start","session_id":"e18a20ff-4c8c-4b94-867b-dd4b79032a6c"}"#,
            "\x07",
        );
        apply_signals(&mut st, sniffer.feed(claude.as_bytes()));
        assert_eq!(
            st.agent_session.as_ref().unwrap().session_id.as_deref(),
            Some("01a0d21f-2f79-72e0-bb16-d6d908aaa6e0"),
            "a Claude hook on an omp pane must not steal the session id"
        );

        apply_agent(&mut st, Some((CLIAgent::Claude, vec!["claude".into()])));
        assert!(
            st.agent_session.is_none(),
            "switching the foreground agent drops the previous session"
        );
    }

    #[test]
    fn a_kitty_notification_marks_a_hookless_agent_waiting() {
        use crate::core::cli_agent::{AgentStatus, CLIAgent};

        let mut st = test_state(true);
        st.agent = Some(CLIAgent::Claude);
        let mut sniffer = OscSniffer::new();
        apply_signals(
            &mut st,
            sniffer.feed(
                b"\x1b]99;i=7:d=0:p=title;Claude Code\x07\x1b]99;i=7:p=body;Claude needs your permission\x07\x1b]99;i=7:d=1:a=focus;\x07",
            ),
        );
        let sess = st.agent_session.clone().unwrap();
        assert_eq!(sess.status, AgentStatus::Waiting);
        assert_eq!(
            sess.message.as_deref(),
            Some("Claude needs your permission")
        );
    }

    #[test]
    fn opaque_notifications_only_fall_back_when_no_rich_state() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        let mut st = test_state(true);
        let mut sniffer = OscSniffer::new();
        apply_signals(&mut st, sniffer.feed(b"\x1b]9;Build finished\x07"));
        assert!(st.agent_session.is_none());

        st.agent = Some(CLIAgent::Codex);
        apply_signals(
            &mut st,
            sniffer.feed(b"\x1b]9;Codex wants to run tests\x07"),
        );
        let sess = st.agent_session.clone().unwrap();
        assert_eq!(sess.status, AgentStatus::Waiting);
        assert!(!sess.rich);

        st.agent_session = Some(AgentSessionState {
            status: AgentStatus::Working,
            message: None,
            session_id: Some("sid".into()),
            launch_argv: None,
            rich: true,
            cwd: None,
            activity: 0,
            turns: 0,
            inferred: false,
            readout: Default::default(),
            since_ms: None,
        });
        apply_signals(&mut st, sniffer.feed(b"\x1b]9;noise\x07"));
        assert_eq!(
            st.agent_session.as_ref().unwrap().status,
            AgentStatus::Working
        );
    }

    #[test]
    fn probed_cwd_corrects_a_pane_whose_shell_never_reports_osc7() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/Users/alice"));
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/Users/alice/dev/tty7")));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p.as_path() == Path::new("/Users/alice/dev/tty7"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn probed_cwd_keeps_the_shells_spelling_for_a_symlinked_path() {
        let tmp = std::env::temp_dir().join(format!("tty7-cwd-{}", std::process::id()));
        let real = tmp.join("real");
        let link = tmp.join("link");
        std::fs::create_dir_all(&real).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut st = test_state(true);
        st.cwd = Some(link.clone());
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(real.canonicalize().unwrap()));

        assert_eq!(st.cwd.as_deref(), Some(link.as_path()));
        assert!(rx.try_recv().is_err(), "same directory → nothing to tell");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn probed_cwd_matching_the_reported_one_says_nothing() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/Users/alice/dev/tty7"));
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/Users/alice/dev/tty7")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn probed_cwd_declines_to_speak_for_a_remote_pane() {
        let mut st = test_state(true);
        st.remote = Some(RemoteContext {
            kind: RemoteKind::Ssh,
            argv: vec!["ssh".into(), "build-box".into()],
            target: "build-box".into(),
        });
        st.cwd = None;
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd, None);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn probed_cwd_absent_leaves_the_reported_cwd_alone() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/work"));
        let (tx, rx) = output_pipe();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, None);

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/work")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resize_echoes_size_to_the_controller_between_old_and_new_output() {
        let mut st = test_state(true);
        let (tx, rx) = output_pipe();
        subscribe(&mut st, tx);

        let pane_gate = OutputGate::new();
        fan_out_output(&mut st, b"old-width bytes", Vec::new(), &pane_gate);
        resize_state(&mut st, ws(120, 30));
        fan_out_output(&mut st, b"new-width bytes", Vec::new(), &pane_gate);

        match rx.try_recv().unwrap() {
            DaemonMsg::Output(b) => assert_eq!(b, b"old-width bytes"),
            other => panic!("expected the pre-resize Output first, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            DaemonMsg::Size(size) => assert_eq!((size.cols, size.rows), (120, 30)),
            other => panic!("expected the Size echo in stream order, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            DaemonMsg::Output(b) => assert_eq!(b, b"new-width bytes"),
            other => panic!("expected the post-resize Output last, got {other:?}"),
        }
    }

    #[test]
    fn a_subscriber_holds_the_reap_and_a_bare_pane_does_not() {
        let bare = Arc::new(Mutex::new(test_state(true)));
        let (dead_tx, dead_rx) = mpsc::channel();
        DeathReporter::new(move || dead_tx.send(()).unwrap())
            .report(&bare, &AtomicBool::new(false));
        assert!(
            dead_rx.try_recv().is_ok(),
            "a pane with no subscriber is reaped at once"
        );

        let held = Arc::new(Mutex::new(test_state(true)));
        let (tx, rx) = output_pipe();
        subscribe(&mut held.lock().unwrap(), tx);
        let (dead_tx, dead_rx) = mpsc::channel();
        DeathReporter::new(move || dead_tx.send(()).unwrap())
            .report(&held, &AtomicBool::new(false));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            dead_rx.try_recv().is_err(),
            "an attached death waits for the subscriber to go"
        );
    }

    #[test]
    fn reader_rejects_every_clipboard_write() {
        use base64::Engine as _;

        let state = Arc::new(Mutex::new(test_state(true)));
        let (tx, rx) = output_pipe();
        subscribe(&mut state.lock().unwrap(), tx);

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = PtyWriter::spawn(Box::new(SharedBuf(sink.clone())));
        let mime = base64::engine::general_purpose::STANDARD.encode("image/png");
        let data = base64::engine::general_purpose::STANDARD.encode(b"png-data");
        let stream = format!(
            "\x1b]5522;type=write:id=nope\x1b\\\
             \x1b]5522;type=wdata:mime={mime};{data}\x1b\\\
             \x1b]5522;type=wdata\x1b\\"
        );
        DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(stream.into_bytes())),
            writer.clone(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        )
        .join()
        .unwrap();
        assert!(
            writer.wait_idle(Duration::from_secs(2)),
            "the clipboard rejection is written on the pty thread"
        );

        let msgs: Vec<_> = rx.try_iter().collect();
        assert!(
            !msgs.iter().any(|msg| matches!(msg, DaemonMsg::Output(_))),
            "a rejected clipboard write leaves no text on screen"
        );
        assert_eq!(
            *sink.lock().unwrap(),
            crate::core::clipboard::response(Some("nope"), "EPERM")
        );
    }

    /// Text on either side of an OSC 5522 write still reaches the subscriber.
    /// The write itself does not: its bytes are not a frame, and the pty gets
    /// `EPERM`.
    #[test]
    fn reader_strips_clipboard_writes_and_keeps_the_text_around_them() {
        use base64::Engine as _;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let state = Arc::new(Mutex::new(test_state(true)));
        let (tx, rx) = output_pipe();
        subscribe(&mut state.lock().unwrap(), tx);
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = PtyWriter::spawn(Box::new(SharedBuf(sink.clone())));

        let mime = base64::engine::general_purpose::STANDARD.encode("image/png");
        let data = base64::engine::general_purpose::STANDARD.encode(b"png-data");
        let stream = format!(
            "before\x1b]5522;type=write:id=req-1\x1b\\\
             \x1b]5522;type=wdata:mime={mime};{data}\x1b\\\
             \x1b]5522;type=wdata\x1b\\after"
        );
        DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(stream.into_bytes())),
            writer.clone(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        )
        .join()
        .unwrap();
        assert!(writer.wait_idle(Duration::from_secs(2)));

        let mut text = Vec::new();
        for msg in rx.try_iter() {
            match msg {
                DaemonMsg::Output(bytes) => text.extend(bytes),
                // The cursor hits EOF once the fixture is consumed, and an
                // attached reader publishes that as `Exited`. It is not the write.
                DaemonMsg::Exited { code: None } => {}
                other => panic!("clipboard write must not be forwarded, got {other:?}"),
            }
        }
        assert_eq!(text, b"beforeafter");
        assert_eq!(
            *sink.lock().unwrap(),
            crate::core::clipboard::response(Some("req-1"), "EPERM")
        );
    }

    #[test]
    fn agent_state_snapshot_reports_only_panes_with_a_session() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        let mut st = test_state(true);
        st.id = 42;
        assert_eq!(agent_state_snapshot(&st), None);

        st.agent = Some(CLIAgent::Claude);
        assert_eq!(
            agent_state_snapshot(&st),
            None,
            "a detected agent without session state is not yet a fact worth listing"
        );

        st.agent_session = Some(AgentSessionState {
            status: AgentStatus::Waiting,
            session_id: Some("sess-1".into()),
            ..Default::default()
        });
        let snap = agent_state_snapshot(&st).expect("a session state is the fact");
        assert_eq!(snap.pane_id, 42);
        assert_eq!(snap.agent, Some(CLIAgent::Claude));
        assert_eq!(snap.state.status, AgentStatus::Waiting);
        assert_eq!(snap.state.session_id.as_deref(), Some("sess-1"));
    }

    /// The foreground probe only ever runs on output, and the prompt a shell
    /// draws after a command is the last output a pane produces until the user
    /// types again. So an `ssh` that exited inside the poll interval used to
    /// leave the pane reporting itself as remote indefinitely: nothing came
    /// along to probe on. A prompt mark now forces the probe (#817).
    #[test]
    fn a_prompt_mark_reprobes_the_foreground_inside_the_poll_interval() {
        /// Hands the reader one chunk per `read`, so two prompt marks arrive as
        /// two passes through the loop a few microseconds apart — well inside
        /// `REMOTE_CONTEXT_POLL_INTERVAL`.
        struct Chunks(std::collections::VecDeque<Vec<u8>>);

        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Ok(0),
                }
            }
        }

        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);

        let probes_taken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let taken = probes_taken.clone();
        let remote = Box::new(move || {
            // First probe: `ssh` holds the pty. Every one after: it is gone.
            (taken.fetch_add(1, Ordering::SeqCst) == 0).then(|| RemoteContext {
                kind: RemoteKind::Ssh,
                argv: vec!["ssh".into(), "box".into()],
                target: "box".into(),
            })
        });

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(Chunks(
                [
                    b"\x1b]133;C;ssh box\x07".to_vec(),
                    b"\x1b]133;D;0\x07".to_vec(),
                ]
                .into_iter()
                .collect(),
            )),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote,
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        assert_eq!(
            probes_taken.load(Ordering::SeqCst),
            2,
            "the prompt mark did not force a second probe"
        );
        assert!(
            state.lock().unwrap().remote.is_none(),
            "the pane still reports the ssh session it has already left"
        );
        let reported: Vec<Option<String>> = sub_rx
            .try_iter()
            .filter_map(|msg| match msg {
                DaemonMsg::RemoteContext(ctx) => Some(ctx.map(|c| c.target)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reported,
            vec![Some("box".to_string()), None],
            "the client was never told the pane came home"
        );
    }

    #[test]
    fn reader_eof_with_subscriber_sends_exited_not_on_dead() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"tail".to_vec())),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"tail"));
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            !dead.load(Ordering::SeqCst),
            "an attached death is the detach path's to reclaim, not on_dead's"
        );
    }

    #[cfg(windows)]
    #[test]
    fn closing_conpty_waits_for_delayed_reader_output_before_exit() {
        /// A master whose destruction opens the simulated ConPTY output pipe.
        /// The production `ConPtyMasterPty` performs the equivalent transition
        /// by calling `ClosePseudoConsole` from its destructor.
        struct SignallingMaster {
            released: Arc<(Mutex<bool>, Condvar)>,
        }

        impl Drop for SignallingMaster {
            fn drop(&mut self) {
                let (lock, ready) = &*self.released;
                *lock.lock().unwrap() = true;
                ready.notify_all();
            }
        }

        impl MasterPty for SignallingMaster {
            fn resize(&self, _size: PtySize) -> anyhow::Result<()> {
                Ok(())
            }

            fn get_size(&self) -> anyhow::Result<PtySize> {
                Ok(PtySize::default())
            }

            fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
                Ok(Box::new(std::io::empty()))
            }

            fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
                Ok(Box::new(std::io::sink()))
            }
        }

        /// Models a reader that stays delayed well past any plausible grace
        /// period, then receives a final output chunk followed by EOF.
        struct DelayedTailReader {
            released: Arc<(Mutex<bool>, Condvar)>,
            tail: std::io::Cursor<Vec<u8>>,
            delayed: bool,
        }

        impl Read for DelayedTailReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.delayed {
                    let (lock, ready) = &*self.released;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                    drop(released);
                    std::thread::sleep(Duration::from_millis(800));
                    self.delayed = true;
                }
                self.tail.read(buf)
            }
        }

        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let master: Mutex<Option<Box<dyn MasterPty + Send>>> =
            Mutex::new(Some(Box::new(SignallingMaster {
                released: released.clone(),
            })));
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);

        let reader = DaemonPane::spawn_reader(
            state,
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(DelayedTailReader {
                released,
                tail: std::io::Cursor::new(b"final output".to_vec()),
                delayed: false,
            }),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );

        close_pty_master(&master);
        assert!(
            sub_rx.recv_timeout(Duration::from_millis(600)).is_err(),
            "closing the master must not publish Exited while the reader is still delayed"
        );
        assert!(matches!(
            sub_rx.recv_timeout(Duration::from_secs(1)),
            Ok(DaemonMsg::Output(bytes)) if bytes == b"final output"
        ));
        assert!(matches!(
            sub_rx.recv_timeout(Duration::from_secs(1)),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(sub_rx.try_recv().is_err(), "no output may follow Exited");
        reader.join().unwrap();
    }

    /// An inert stand-in for the ConPTY master: releasing it is instant, which
    /// is what `ClosePseudoConsole` does once the pipe has no pending bytes.
    #[cfg(windows)]
    struct InertMaster;

    #[cfg(windows)]
    impl MasterPty for InertMaster {
        fn resize(&self, _size: PtySize) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }

        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }
    }

    #[cfg(windows)]
    fn exit_drain_fixture() -> (
        Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        Arc<Mutex<PaneState>>,
        Arc<AtomicBool>,
        Arc<DeathReporter>,
        OutputRx,
    ) {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);
        (
            Arc::new(Mutex::new(Some(
                Box::new(InertMaster) as Box<dyn MasterPty + Send>
            ))),
            state,
            Arc::new(AtomicBool::new(false)),
            Arc::new(DeathReporter::new(|| {})),
            sub_rx,
        )
    }

    /// A grandchild that inherited the ConPTY output pipe keeps it open after
    /// the shell is gone, so the reader never sees EOF and can never publish
    /// the exit. The monitor's drain window is the only thing that stops such
    /// a pane from reading as alive forever.
    #[cfg(windows)]
    #[test]
    fn a_pty_that_never_reaches_eof_still_reports_the_exit() {
        struct NeverEofReader;

        impl Read for NeverEofReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                // Long enough to outlast the window below, bounded so the
                // thread cannot outlive the test binary.
                std::thread::sleep(Duration::from_secs(5));
                Ok(0)
            }
        }

        let (master, state, shutting_down, death, sub_rx) = exit_drain_fixture();
        let _reader = DaemonPane::spawn_reader(
            state.clone(),
            shutting_down.clone(),
            Arc::new(OutputGate::new()),
            Box::new(NeverEofReader),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            death.clone(),
        );

        drain_then_report(
            master,
            state,
            shutting_down,
            death,
            Duration::from_millis(50),
        );
        assert!(
            matches!(
                sub_rx.recv_timeout(Duration::from_secs(1)),
                Ok(DaemonMsg::Exited { code: None })
            ),
            "the monitor must report the exit once its drain window elapses"
        );
    }

    /// The ordinary case: EOF arrives, the reader reports, and the monitor
    /// neither waits out its window nor publishes a second `Exited`.
    #[cfg(windows)]
    #[test]
    fn the_exit_monitor_defers_to_the_reader_that_saw_eof() {
        let (master, state, shutting_down, death, sub_rx) = exit_drain_fixture();
        let reader = DaemonPane::spawn_reader(
            state.clone(),
            shutting_down.clone(),
            Arc::new(OutputGate::new()),
            Box::new(std::io::empty()),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            death.clone(),
        );

        let started = std::time::Instant::now();
        drain_then_report(master, state, shutting_down, death, Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the monitor must stop waiting as soon as the reader has reported"
        );
        assert!(matches!(
            sub_rx.recv_timeout(Duration::from_secs(1)),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            sub_rx.try_recv().is_err(),
            "the idempotent reporter must not publish a second Exited"
        );
        reader.join().unwrap();
    }

    /// Issue #213 end-to-end at the reader: a chunk carrying text plus a
    /// kitty graphics query and a transmit-and-display must (1) keep only the
    /// text in the replay ring and the `Output` frame, (2) write the `a=q` reply
    /// back to the PTY writer, and (3) forward the image out-of-band as an
    /// `Image` frame the client can decode.
    #[test]
    fn reader_strips_graphics_and_forwards_them_out_of_band() {
        use crate::core::kitty_graphics::Image;
        use base64::Engine as _;

        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);

        // A writer we can read back, to prove the query reply reached the PTY.
        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = PtyWriter::spawn(Box::new(SharedBuf(sink.clone())));

        // One 1x1 opaque-red RGBA pixel, transmitted-and-displayed inline.
        let pixel = [0xffu8, 0x00, 0x00, 0xff];
        let b64 = base64::engine::general_purpose::STANDARD.encode(pixel);
        let stream = format!(
            "before\x1b_Gi=1,a=q,t=d,f=32,s=1,v=1;AAAA\x1b\\\
             mid\x1b_Ga=T,f=32,t=d,s=1,v=1,i=1;{b64}\x1b\\after"
        );

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(stream.into_bytes())),
            writer.clone(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();
        assert!(
            writer.wait_idle(Duration::from_secs(2)),
            "the query reply is written on the pty thread"
        );

        // (1) The query reply went to the PTY writer.
        assert_eq!(sink.lock().unwrap().as_slice(), b"\x1b_Gi=1;OK\x1b\\");
        // (3) The subscriber sees the passthrough and the image in stream order.
        // Consecutive text shares one Output; the image is not text, so it
        // stays between the text before it and the text after it. The `a=q`
        // reply goes to the PTY, not onto this pipe.
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"beforemid"));
        match sub_rx.try_recv() {
            Ok(DaemonMsg::Image(frame)) => {
                let mut img =
                    Image::decode_frame_owned(frame.clone()).expect("decodable image frame");
                assert_eq!(img.id, 1);
                assert_eq!((img.width, img.height), (1, 1));
                assert_eq!(img.take_rgba8().unwrap(), pixel);
            }
            other => panic!("expected Image frame, got {other:?}"),
        }
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"after"));
    }

    /// A kitty delete (`a=d`) is lifted out and forwarded as a `DeleteImage`
    /// selector, leaving the surrounding text intact for the subscriber.
    #[test]
    fn reader_forwards_graphics_deletes() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"x\x1b_Ga=d,d=A\x1b\\y".to_vec())),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        // In stream order: text before the delete, the delete, then text after.
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"x"));
        match sub_rx.try_recv() {
            Ok(DaemonMsg::DeleteImage(sel)) => {
                let d = ImageDelete::decode(&sel).unwrap();
                assert_eq!(d.target, b'A');
            }
            other => panic!("expected DeleteImage, got {other:?}"),
        }
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"y"));
    }

    #[test]
    fn push_image_frame_drops_over_max_frame() {
        let mut frames = Vec::new();
        // At the limit: queued.
        push_image_frame(&mut frames, vec![0u8; MAX_FRAME]);
        // Over the limit: dropped, so it never enters the subscriber queue.
        push_image_frame(&mut frames, vec![0u8; MAX_FRAME + 1]);
        assert_eq!(frames.len(), 1, "only the in-budget frame is queued");
        assert!(matches!(&frames[0], GraphicsFrame::Image(f) if f.len() == MAX_FRAME));
    }

    #[test]
    fn reader_poll_applies_the_probed_cwd() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        {
            let mut st = state.lock().unwrap();
            st.cwd = Some(PathBuf::from("/Users/alice"));
            st.subscriber = Some(sub_tx);
        }

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"alice@host ~ % ".to_vec())),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| Some(PathBuf::from("/Users/alice/dev/tty7"))),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        assert_eq!(
            state.lock().unwrap().cwd.as_deref(),
            Some(Path::new("/Users/alice/dev/tty7"))
        );
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(_))));
        assert!(
            matches!(sub_rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p.as_path() == Path::new("/Users/alice/dev/tty7"))
        );
    }

    #[test]
    fn reader_eof_without_subscriber_fires_on_dead() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (dead_tx, dead_rx) = mpsc::channel();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(Vec::new())),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || dead_tx.send(()).unwrap())),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(dead_rx.try_recv().is_ok(), "unattached death → on_dead");
    }

    #[test]
    fn reader_eof_during_shutdown_is_silent() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(Vec::new())),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(!dead.load(Ordering::SeqCst));
    }

    #[test]
    fn death_reporter_notifies_once_across_racing_callers() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let shutting_down = AtomicBool::new(false);
        let calls = Arc::new(AtomicBool::new(false));
        let calls_flag = calls.clone();
        let death = DeathReporter::new(move || calls_flag.store(true, Ordering::SeqCst));

        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(!state.lock().unwrap().alive);
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            sub_rx.try_recv().is_err(),
            "a second report must not re-notify"
        );
    }

    #[test]
    fn death_reporter_fires_on_dead_at_most_once() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let shutting_down = AtomicBool::new(false);
        let (dead_tx, dead_rx) = mpsc::channel();
        let death = DeathReporter::new(move || dead_tx.send(()).unwrap());

        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(dead_rx.try_recv().is_ok(), "unattached death → on_dead");
        assert!(dead_rx.try_recv().is_err(), "on_dead must fire only once");
    }

    #[test]
    fn pane_environment_advertises_the_terminal_under_the_standard_names() {
        let env: std::collections::HashMap<_, _> =
            pane_environment(&std::collections::HashMap::new(), false, 7, "/bin/zsh")
                .into_iter()
                .collect();
        let version = env!("CARGO_PKG_VERSION");

        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("tty7"));
        assert_eq!(env.get("FORCE_HYPERLINK").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("TERM_PROGRAM_VERSION").map(String::as_str),
            Some(version)
        );
        assert_eq!(env.get(NTTY7_ENV_MARKER).map(String::as_str), Some(version));
        assert_eq!(
            env.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "terminfo name is what the pane's decoder actually implements"
        );
    }

    #[test]
    fn pane_environment_hands_the_shell_its_own_address() {
        let env: std::collections::HashMap<_, _> =
            pane_environment(&std::collections::HashMap::new(), false, 42, "/bin/zsh")
                .into_iter()
                .collect();

        assert_eq!(
            env.get(NTTY7_PANE_ENV).map(String::as_str),
            Some("42"),
            "a CLI inside the pane needs its own pane id for address-free verbs"
        );
        match config_dir_env() {
            Some(dir) => assert_eq!(
                env.get(NTTY7_CONFIG_DIR_ENV),
                Some(&dir),
                "the shell is told which config dir this server runs on, so a CLI \
                 there resolves both endpoints the same way the server opened them"
            ),
            None => assert!(
                !env.contains_key(NTTY7_CONFIG_DIR_ENV),
                "no resolvable config dir must not inject an empty NTTY7_CONFIG_DIR"
            ),
        }
    }

    #[cfg(unix)]
    #[test]
    fn pane_environment_names_the_shell_the_pane_actually_runs() {
        // The reported bug: a pane running fish still advertised the passwd
        // login shell, so tmux, `sudo -s` and agents inside it all picked zsh.
        let env: std::collections::HashMap<_, _> = pane_environment(
            &std::collections::HashMap::new(),
            false,
            1,
            "/opt/homebrew/bin/fish",
        )
        .into_iter()
        .collect();

        assert_eq!(
            env.get(SHELL_ENV).map(String::as_str),
            Some("/opt/homebrew/bin/fish"),
            "$SHELL must name the shell the pane is actually running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unconfigured_pane_still_announces_the_shell_it_runs() {
        // No configured shell means the pane runs the login shell, and naming
        // it is both harmless and correct: it is what the pane is running.
        let cmd = build_shell_command(None, &Some(PathBuf::from("/tmp")), 1)
            .expect("build default shell command")
            .0;
        let shell = cmd
            .get_env(SHELL_ENV)
            .and_then(|v| v.to_str())
            .map(str::to_string)
            .expect("the default shell resolves to an absolute path");

        assert!(
            shell.starts_with('/'),
            "$SHELL must be an absolute path, got {shell}"
        );
        assert_eq!(
            shell,
            default_shell_name(&CommandBuilder::new_default_prog()),
            "an unconfigured pane announces the very shell it was about to run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_shell_variable_outranks_the_shell_tty7_launched() {
        // Same precedence contract as TERM_PROGRAM: tty7 describes the pane,
        // the user's own `env` block gets the last word.
        let configured = [("SHELL".to_string(), "/bin/ksh".to_string())]
            .into_iter()
            .collect();

        let applied: std::collections::HashMap<_, _> =
            pane_environment(&configured, false, 1, "/opt/homebrew/bin/fish")
                .into_iter()
                .collect();

        assert_eq!(applied.get(SHELL_ENV).map(String::as_str), Some("/bin/ksh"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_panes_are_left_without_a_shell_variable() {
        let applied = pane_environment(
            &std::collections::HashMap::new(),
            false,
            1,
            r"C:\Program Files\PowerShell\7\pwsh.exe",
        );

        assert!(
            !applied
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case(SHELL_ENV)),
            "$SHELL is a POSIX contract: the MSYS, Cygwin and WSL tools that \
             read it on Windows want a POSIX path, and a Windows one would \
             point them at a shell they cannot exec"
        );
    }

    #[test]
    fn shell_env_path_only_ever_answers_an_absolute_path() {
        let never = |_: &str| false;
        let path = || Some("/usr/local/bin:/usr/bin:relative/bin".to_string());

        assert_eq!(
            shell_env_path("/opt/homebrew/bin/fish", path, never).as_deref(),
            Some("/opt/homebrew/bin/fish"),
            "an absolute program is already the answer, PATH untouched"
        );
        assert_eq!(
            shell_env_path("fish", path, |c| c == "/usr/local/bin/fish").as_deref(),
            Some("/usr/local/bin/fish"),
            "a bare command must be resolved the way exec would resolve it"
        );
        assert_eq!(
            shell_env_path("fish", path, never),
            None,
            "an unresolvable bare command must leave $SHELL alone rather than \
             hand a consumer a name it may resolve differently"
        );
        assert_eq!(
            shell_env_path("./fish", path, |_| true),
            None,
            "a cwd-relative program is meaningless to a process with another cwd"
        );
        assert_eq!(shell_env_path("  ", path, |_| true), None);
        assert_eq!(
            shell_env_path("fish", || None, |_| true),
            None,
            "no PATH to search means nothing to promise"
        );
    }

    #[test]
    fn the_shell_we_announce_is_the_argv_we_are_about_to_exec() {
        let mut configured = CommandBuilder::new("/opt/homebrew/bin/fish");
        configured.args(["-l"]);
        assert_eq!(
            launched_shell_program(&configured, "/bin/zsh"),
            "/opt/homebrew/bin/fish",
            "a builder with an argv has already overruled whatever tty7 resolved"
        );

        assert_eq!(
            launched_shell_program(&CommandBuilder::new_default_prog(), "/bin/zsh"),
            "/bin/zsh",
            "an untouched default prog is exactly what resolved_program describes"
        );
    }

    #[test]
    fn shell_env_path_ignores_relative_path_entries() {
        // A relative PATH entry resolves against the *pane's* cwd, which is not
        // ours, so it can never yield the absolute path $SHELL must hold.
        assert_eq!(
            shell_env_path("fish", || Some("bin:.".to_string()), |_| true),
            None
        );
    }

    #[test]
    fn pane_environment_lets_configured_env_override_identity_but_not_capability() {
        let configured = [
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.5.0"),
            ("TERM", "dumb"),
            ("COLORTERM", ""),
            ("EDITOR", "hx"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();

        let applied: std::collections::HashMap<_, _> =
            pane_environment(&configured, false, 1, "/bin/zsh")
                .into_iter()
                .collect();

        assert_eq!(
            applied.get("TERM_PROGRAM").map(String::as_str),
            Some("iTerm.app")
        );
        assert_eq!(
            applied.get("TERM_PROGRAM_VERSION").map(String::as_str),
            Some("3.5.0")
        );
        assert_eq!(applied.get("EDITOR").map(String::as_str), Some("hx"));
        assert_eq!(
            applied.get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(
            applied.get("COLORTERM").map(String::as_str),
            Some("truecolor")
        );
    }

    #[cfg(windows)]
    #[test]
    fn pane_environment_advertises_light_and_dark_backgrounds() {
        let empty = std::collections::HashMap::new();
        let light: std::collections::HashMap<_, _> = pane_environment(&empty, false, 1, "pwsh.exe")
            .into_iter()
            .collect();
        let dark: std::collections::HashMap<_, _> = pane_environment(&empty, true, 1, "pwsh.exe")
            .into_iter()
            .collect();

        assert_eq!(light.get("COLORFGBG").map(String::as_str), Some("0;15"));
        assert_eq!(dark.get("COLORFGBG").map(String::as_str), Some("15;0"));
    }

    #[cfg(windows)]
    #[test]
    fn configured_colorfgbg_wins_case_insensitively() {
        let configured = [("ColorFgBg".to_string(), "3;4".to_string())]
            .into_iter()
            .collect();
        let applied = pane_environment(&configured, false, 1, "pwsh.exe");

        assert!(!applied.iter().any(|(key, _)| key == "COLORFGBG"));
        assert!(
            applied
                .iter()
                .any(|(key, value)| key == "ColorFgBg" && value == "3;4")
        );
    }

    #[cfg(windows)]
    #[test]
    fn pane_environment_capability_keys_cannot_be_overridden_by_recasing() {
        let configured = [("Term", "dumb"), ("ColorTerm", ""), ("term_program", "x")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();

        let applied = pane_environment(&configured, false, 1, "pwsh.exe");

        assert!(
            !applied.iter().any(|(k, _)| k == "Term" || k == "ColorTerm"),
            "a recased capability key must be filtered out, or it would land \
             in the same case-folded slot and win by coming later"
        );
        let get = |key: &str| {
            applied
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("TERM"), Some("xterm-256color"));
        assert_eq!(get("COLORTERM"), Some("truecolor"));
        assert_eq!(get("term_program"), Some("x"));
    }

    #[test]
    fn locale_fallback_respects_inherited_and_configured_environments() {
        let environment = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let fallback_is_needed = |extra: &[(&str, &str)], parent: &[(&str, &str)]| {
            let extra = environment(extra);
            let parent = environment(parent);
            locale_fallback_is_needed(&extra, |key| parent.get(key).cloned())
        };

        assert!(fallback_is_needed(&[], &[]));
        assert!(fallback_is_needed(&[], &[("LANG", "")]));
        assert!(!fallback_is_needed(&[], &[("LANG", "en_US.UTF-8")]));
        assert!(!fallback_is_needed(&[], &[("LC_ALL", "C")]));
        assert!(!fallback_is_needed(&[("LC_CTYPE", "UTF-8")], &[]));
        assert!(!fallback_is_needed(&[("LC_CTYPE", "")], &[]));
        assert!(!fallback_is_needed(
            &[("LANG", "")],
            &[("LANG", "en_US.UTF-8")]
        ));
    }

    #[test]
    fn locale_fallback_sets_a_variable_that_backs_every_category() {
        let injected =
            std::iter::once((LOCALE_FALLBACK_KEY.to_string(), "en_US.UTF-8".to_string()))
                .collect::<std::collections::HashMap<_, _>>();

        // Whatever we inject has to satisfy the check that gated it, or every
        // pane would keep re-deriving a fallback that is already in place.
        assert!(!locale_fallback_is_needed(&injected, |_| None));

        // And it has to back every category, not just character handling — see
        // LOCALE_FALLBACK_KEY. LC_CTYPE here would leave a shell warning about
        // LC_COLLATE and friends on every launch.
        assert_eq!(LOCALE_FALLBACK_KEY, "LANG");
    }

    #[test]
    fn posix_locale_stem_drops_script_and_keyword_subtags() {
        let stem = |id: &str| posix_locale_stem(id);

        assert_eq!(stem("en_US").as_deref(), Some("en_US"));
        assert_eq!(stem("en-US").as_deref(), Some("en_US"));
        assert_eq!(stem("zh_Hans_CN").as_deref(), Some("zh_CN"));
        assert_eq!(
            stem("zh_Hans_CN@calendar=gregorian").as_deref(),
            Some("zh_CN")
        );
        assert_eq!(stem("es_419").as_deref(), Some("es_419"));
        assert_eq!(stem("EN_us").as_deref(), Some("en_US"));

        assert_eq!(stem("zh"), None);
        assert_eq!(stem("zh_Hans"), None);
        assert_eq!(stem(""), None);
        assert_eq!(stem("@calendar=gregorian"), None);
    }

    #[test]
    fn character_locale_only_returns_installed_locales() {
        let installed = |names: &'static [&'static str]| move |n: &str| names.contains(&n);

        assert_eq!(
            character_locale(Some("zh_Hans_CN"), installed(&["zh_CN.UTF-8", "C.UTF-8"])),
            Some("zh_CN.UTF-8".to_string())
        );
        assert_eq!(
            character_locale(Some("en_CN"), installed(&["C.UTF-8", "en_US.UTF-8"])),
            Some("C.UTF-8".to_string())
        );
        assert_eq!(
            character_locale(Some("en_CN"), installed(&["en_US.UTF-8"])),
            Some("en_US.UTF-8".to_string())
        );
        assert_eq!(
            character_locale(None, installed(&["C.UTF-8", "en_US.UTF-8"])),
            Some("C.UTF-8".to_string())
        );
        assert_eq!(character_locale(Some("zh_Hans_CN"), installed(&[])), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn system_locale_identifier_is_present() {
        assert!(system_locale_identifier().is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn derived_character_locale_is_installed_on_this_machine() {
        let exists = |name: &str| {
            std::path::Path::new(LOCALE_DEFINITION_DIR)
                .join(name)
                .is_dir()
        };
        let locale = character_locale(system_locale_identifier().as_deref(), exists)
            .expect("macOS always ships en_US.UTF-8");
        assert!(exists(&locale), "derived {locale} is not installed");
        assert!(
            locale.ends_with(".UTF-8"),
            "derived {locale} is not a UTF-8 locale"
        );
    }

    #[test]
    fn spawned_shell_carries_the_tty7_marker() {
        let cmd = build_shell_command(None, &Some(PathBuf::from("/tmp")), 42)
            .expect("build default shell command")
            .0;
        let tty7 = cmd.get_env(NTTY7_ENV_MARKER).and_then(|v| v.to_str());
        assert_eq!(
            tty7,
            Some(env!("CARGO_PKG_VERSION")),
            "the daemon must inject TTY7 into every spawned shell"
        );
        assert_eq!(
            cmd.get_env(NTTY7_PANE_ENV).and_then(|v| v.to_str()),
            Some("42"),
            "the daemon must tell every spawned shell which pane it is"
        );
        if let Some(dir) = config_dir_env() {
            assert_eq!(
                cmd.get_env(NTTY7_CONFIG_DIR_ENV).and_then(|v| v.to_str()),
                Some(dir.as_str()),
                "the daemon must tell every spawned shell which config dir it serves"
            );
        }
    }

    #[test]
    fn apply_signals_updates_state() {
        let mut st = test_state(true);

        apply_signals(
            &mut st,
            SniffSignals {
                cwd: Some(PathBuf::from("/tmp/x")),
                ..SniffSignals::default()
            },
        );
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));

        apply_signals(
            &mut st,
            SniffSignals {
                shell: vec![ShellState {
                    active: true,
                    at_prompt: true,
                    command: None,
                }],
                ..SniffSignals::default()
            },
        );
        assert!(st.shell.active && st.shell.at_prompt);

        apply_signals(&mut st, SniffSignals::default());
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));
    }

    /// A quick launch types the agent's command the moment the shell is up, so
    /// the agent's whole first screen can land inside the poll interval of the
    /// prompt it was typed at — and then it waits for a key, printing nothing
    /// more. Probing only on the interval, the pane never found out an agent
    /// was running. A change of foreground process group now probes at once.
    #[test]
    fn a_new_foreground_group_is_probed_inside_the_poll_interval() {
        struct Chunks(std::collections::VecDeque<Vec<u8>>);

        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Ok(0),
                }
            }
        }

        let probes_for = |groups: Vec<i32>| {
            let state = Arc::new(Mutex::new(test_state(true)));
            let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = probes.clone();
            let groups = Mutex::new(groups.into_iter());
            let handle = DaemonPane::spawn_reader(
                state,
                Arc::new(AtomicBool::new(false)),
                Arc::new(OutputGate::new()),
                // Plain output, no prompt marks: only the interval or the
                // group can trigger a probe.
                Box::new(Chunks(
                    [b"$ ".to_vec(), b"agent screen".to_vec()]
                        .into_iter()
                        .collect(),
                )),
                null_writer(),
                || false,
                ForegroundProbes {
                    group: Box::new(move || groups.lock().unwrap().next()),
                    remote: Box::new(|| None),
                    agent: Box::new(move || {
                        counted.fetch_add(1, Ordering::SeqCst);
                        None
                    }),
                    cwd: Box::new(|| None),
                },
                Arc::new(DeathReporter::new(|| {})),
            );
            handle.join().unwrap();
            probes.load(Ordering::SeqCst)
        };

        assert_eq!(
            probes_for(vec![10, 10]),
            1,
            "same program: the interval rules"
        );
        assert_eq!(
            probes_for(vec![10, 11]),
            2,
            "a new program is probed at once"
        );
    }

    /// A plain burst samples the foreground group twice (the first read, then
    /// one confirming read) and then stops. A cwd report and an OSC 133 `C`
    /// ask again at once, and the read after `C` asks even though the group
    /// had not moved yet — the child takes the tty on that next chunk.
    #[test]
    fn a_settled_foreground_group_is_not_asked_on_every_read() {
        struct Chunks(std::collections::VecDeque<Vec<u8>>);

        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Ok(0),
                }
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in = calls.clone();
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probes_in = probes.clone();
        // One value per ioctl, not per read: the third and fourth flood
        // chunks must not ask. They are a full flood packet; a short read
        // would ask every time.
        let groups = Mutex::new([7, 7, 8, 8, 8, 9].into_iter());
        let flood = || vec![b'a'; GROUP_FLOOD_BYTES];
        let handle = DaemonPane::spawn_reader(
            Arc::new(Mutex::new(test_state(true))),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(Chunks(
                [
                    flood(),
                    flood(),
                    flood(),
                    flood(),
                    b"\x1b]7;file://host/tmp\x07".to_vec(),
                    b"still".to_vec(),
                    b"\x1b]133;C;echo\x07".to_vec(),
                    b"child".to_vec(),
                ]
                .into_iter()
                .collect(),
            )),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(move || {
                    calls_in.fetch_add(1, Ordering::SeqCst);
                    groups.lock().unwrap().next()
                }),
                remote: Box::new(|| None),
                agent: Box::new(move || {
                    probes_in.fetch_add(1, Ordering::SeqCst);
                    None
                }),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "two flood reads settle the group; the next two stay on the \
             timer; cwd, the confirming read, the command mark, and the \
             child's first output ask again"
        );
        assert_eq!(
            probes.load(Ordering::SeqCst),
            3,
            "a probe runs on the first sample and on each group change, not on every read"
        );
    }

    /// A Darwin `ONLCR` packet is about 1 KiB, sometimes a few bytes under
    /// that, which is more than [`GROUP_FLOOD_BYTES`] and far less than 4 KiB.
    /// After the group has been seen twice, those packets stay on the timer.
    /// A short prompt still asks at once.
    #[test]
    fn an_onlcr_sized_packet_does_not_query_the_foreground_group_every_read() {
        struct Chunks(std::collections::VecDeque<Vec<u8>>);

        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Ok(0),
                }
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in = calls.clone();
        let packet = || vec![b'a'; 1020];
        let handle = DaemonPane::spawn_reader(
            Arc::new(Mutex::new(test_state(true))),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(Chunks(
                [
                    packet(),
                    packet(),
                    packet(),
                    packet(),
                    packet(),
                    packet(),
                    b"ready $ ".to_vec(),
                    packet(),
                ]
                .into_iter()
                .collect(),
            )),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(move || {
                    calls_in.fetch_add(1, Ordering::SeqCst);
                    Some(7)
                }),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two ~1 KiB packets settle the group, the later packets stay on \
             the timer, and the short prompt asks again"
        );
    }

    /// Prompt, echo, and a child screen, back to back, with no OSC 133 and
    /// no pause long enough to expire the 50 ms timer. Two identical samples
    /// settle the shell. The child's first paint is still a short read, so
    /// the new group — and the agent probe that follows a group change — is
    /// taken on that chunk, not on the next timer.
    #[test]
    fn a_fast_child_without_shell_integration_is_seen_on_its_first_screen() {
        struct Chunks(std::collections::VecDeque<Vec<u8>>);

        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Ok(0),
                }
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_in = calls.clone();
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probes_in = probes.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let groups = Mutex::new([7, 7, 7, 8].into_iter());
        let handle = DaemonPane::spawn_reader(
            Arc::new(Mutex::new(test_state(true))),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(Chunks(
                [
                    b"$ ".to_vec(),
                    b"claude".to_vec(),
                    b"\r".to_vec(),
                    b"Claude ready".to_vec(),
                ]
                .into_iter()
                .collect(),
            )),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(move || {
                    calls_in.fetch_add(1, Ordering::SeqCst);
                    let group = groups.lock().unwrap().next();
                    if let Some(group) = group {
                        seen_in.lock().unwrap().push(group);
                    }
                    group
                }),
                remote: Box::new(|| None),
                agent: Box::new(move || {
                    probes_in.fetch_add(1, Ordering::SeqCst);
                    None
                }),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[7, 7, 7, 8],
            "every short read asks, including the child's first screen"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "the shell's first sample and the child's new group each probe; \
             the echoes in between do not"
        );
    }

    /// A short master read is the whole delivery and must come back without a
    /// second `read`. A full packet drains while `poll` still reports the
    /// master readable, and returns when it does not instead of blocking for
    /// the next packet.
    #[cfg(unix)]
    #[test]
    fn burst_read_returns_a_short_packet_and_drains_only_queued_output() {
        struct OwnedFd(i32);
        impl Drop for OwnedFd {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    unsafe { libc::close(self.0) };
                    self.0 = -1;
                }
            }
        }
        impl Read for OwnedFd {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }

        fn raw_pty() -> (i32, i32) {
            let mut master = -1;
            let mut slave = -1;
            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
            let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
            unsafe { libc::cfmakeraw(&mut termios) };
            assert_eq!(
                unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) },
                0
            );
            (master, slave)
        }

        fn read_one(master: i32, slave: i32) -> Vec<u8> {
            let poll = unsafe { libc::dup(master) };
            assert!(poll >= 0);
            let mut burst = BurstRead {
                inner: Box::new(OwnedFd(master)),
                poll_fd: Some(poll),
            };
            let slave = OwnedFd(slave);
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; READ_BURST];
                let n = burst.read(&mut buf);
                let _ = tx.send(n.map(|n| buf[..n].to_vec()));
            });
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(got)) => {
                    drop(slave);
                    got
                }
                other => {
                    drop(slave);
                    panic!("pty read blocked after bytes were already queued: {other:?}");
                }
            }
        }

        let (master, slave) = raw_pty();
        let payload = b"ready $ ";
        assert_eq!(
            unsafe { libc::write(slave, payload.as_ptr().cast(), payload.len()) },
            payload.len() as isize
        );
        assert_eq!(read_one(master, slave), payload);

        let (master, slave) = raw_pty();
        let flags = unsafe { libc::fcntl(slave, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(slave, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let blob = vec![b'Q'; 8192];
        let mut written = 0;
        while written < blob.len() {
            let n = unsafe {
                libc::write(slave, blob[written..].as_ptr().cast(), blob.len() - written)
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
                break;
            }
            assert!(n > 0);
            written += n as usize;
        }
        assert!(
            written >= PTY_CHUNK,
            "the pty queue held {written} bytes, short of one packet"
        );
        let got = read_one(master, slave);
        assert_eq!(
            got.len(),
            written,
            "a full packet must take the bytes already queued and not stop \
             after the first 1 KiB, nor block for bytes that are not queued"
        );
        assert!(got.iter().all(|b| *b == b'Q'));
    }

    /// `TIOCOUTQ` stays positive after `tcflow(TCOOFF)`, and `read` blocks
    /// until `XON`. The first packet has already been taken; the drain must
    /// see `poll` say not-readable and return it instead of waiting for `Ctrl-Q`.
    #[cfg(unix)]
    #[test]
    fn burst_read_does_not_block_when_xoff_stops_the_output_queue() {
        struct OwnedFd(i32);
        impl Drop for OwnedFd {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    unsafe { libc::close(self.0) };
                    self.0 = -1;
                }
            }
        }

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(
            unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) },
            0
        );
        let flags = unsafe { libc::fcntl(slave, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(slave, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let blob = vec![b'Q'; 8192];
        let mut written = 0;
        while written < blob.len() {
            let n = unsafe {
                libc::write(slave, blob[written..].as_ptr().cast(), blob.len() - written)
            };
            if n < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                break;
            }
            assert!(n > 0);
            written += n as usize;
        }
        assert!(written >= PTY_CHUNK, "queued {written}");
        assert_eq!(unsafe { libc::tcflow(slave, libc::TCOOFF) }, 0);
        assert!(
            !pty_readable(master),
            "XOFF output must not poll as readable"
        );
        assert!(
            queued_pty_output(master).unwrap_or(0) >= PTY_CHUNK,
            "XOFF leaves the output queue occupied"
        );

        // The kernel queue is already stopped, so the real `read` would
        // block. Hand the drain a full packet first; a second `read` is the
        // bug.
        struct FirstPacket {
            fd: i32,
            served: bool,
        }
        impl Read for FirstPacket {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.served {
                    self.served = true;
                    let n = PTY_CHUNK.min(buf.len());
                    buf[..n].fill(b'A');
                    return Ok(n);
                }
                let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }
        impl Drop for FirstPacket {
            fn drop(&mut self) {
                if self.fd >= 0 {
                    unsafe { libc::close(self.fd) };
                    self.fd = -1;
                }
            }
        }

        let poll = unsafe { libc::dup(master) };
        assert!(poll >= 0);
        let probe = unsafe { libc::dup(master) };
        assert!(probe >= 0);
        let mut burst = BurstRead {
            inner: Box::new(FirstPacket {
                fd: master,
                served: false,
            }),
            poll_fd: Some(poll),
        };
        let slave = OwnedFd(slave);
        let probe = OwnedFd(probe);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut buf = vec![0u8; READ_BURST];
            let read = burst.read(&mut buf);
            let queued = queued_pty_output(probe.0).unwrap_or(0);
            let elapsed = started.elapsed();
            let _ = tx.send(read.map(|n| (buf[..n].to_vec(), queued, elapsed)));
            drop(burst);
            drop(probe);
        });
        let reported = rx.recv_timeout(Duration::from_millis(800));
        drop(slave);
        match reported {
            Ok(Ok((got, queued, elapsed))) => {
                assert!(
                    elapsed < Duration::from_millis(500),
                    "drain blocked for {elapsed:?} under XOFF"
                );
                assert_eq!(got, vec![b'A'; PTY_CHUNK]);
                assert!(
                    queued >= PTY_CHUNK,
                    "drain consumed the stopped queue ({queued} left)"
                );
            }
            other => panic!("burst read blocked while output was XOFF'd: {other:?}"),
        }
    }

    /// A slave that keeps writing past one packet has to come back as one
    /// `read`. Each packet wakes the writer; the drain loops while `poll`
    /// says more output is already there.
    #[cfg(unix)]
    #[test]
    fn burst_read_coalesces_while_the_slave_keeps_writing() {
        struct OwnedFd(i32);
        impl Drop for OwnedFd {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    unsafe { libc::close(self.0) };
                    self.0 = -1;
                }
            }
        }
        impl Read for OwnedFd {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    // The slave's `write` refills the 1 KiB queue after this
                    // packet. Give it the cpu before `drain_queued` polls, or
                    // the queue looks empty and the rest waits for the next call.
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(50) && !pty_readable(self.0) {
                        std::thread::yield_now();
                    }
                }
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(
            unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) },
            0
        );

        let total = 8 * 1024;
        let slave_fd = slave;
        let writer = std::thread::spawn(move || {
            let chunk = [b'Z'; 1024];
            let mut left = total;
            while left > 0 {
                let n =
                    unsafe { libc::write(slave_fd, chunk.as_ptr().cast(), chunk.len().min(left)) };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if n == 0 {
                    break;
                }
                left -= n as usize;
            }
            unsafe { libc::close(slave_fd) };
            Ok(total - left)
        });

        let poll = unsafe { libc::dup(master) };
        assert!(poll >= 0);
        let mut burst = BurstRead {
            inner: Box::new(OwnedFd(master)),
            poll_fd: Some(poll),
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_BURST];
            let n = burst.read(&mut buf);
            drop(burst);
            let _ = tx.send(n.map(|n| buf[..n].to_vec()));
        });
        let reported = rx.recv_timeout(Duration::from_secs(2));
        let wrote = writer.join().expect("writer thread");
        match reported {
            Ok(Ok(got)) => {
                let wrote = wrote.expect("slave write");
                assert_eq!(wrote, total);
                assert!(
                    got.len() > 4 * 1024,
                    "one read coalesced only {} bytes",
                    got.len()
                );
                assert!(got.iter().all(|b| *b == b'Z'));
            }
            other => panic!("coalesced read did not return: {other:?}"),
        }
    }

    /// A first packet a few bytes under 1024 is still a full delivery, which
    /// is what Darwin returns when `ONLCR` refuses to split `\r\n`. The drain
    /// has to take whatever else is already queued; stopping at that packet
    /// is the old `PTY_CHUNK == 1024` bug.
    #[cfg(unix)]
    #[test]
    fn burst_read_drains_a_packet_short_of_1024() {
        struct OwnedFd(i32);
        impl Drop for OwnedFd {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    unsafe { libc::close(self.0) };
                    self.0 = -1;
                }
            }
        }
        impl Read for OwnedFd {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(
            unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) },
            0
        );
        let flags = unsafe { libc::fcntl(slave, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(slave, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let blob = vec![b'Q'; 8192];
        let mut written = 0;
        while written < blob.len() {
            let n = unsafe {
                libc::write(slave, blob[written..].as_ptr().cast(), blob.len() - written)
            };
            if n < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                break;
            }
            assert!(n > 0);
            written += n as usize;
        }
        assert!(
            written > PTY_CHUNK,
            "the pty queue held {written}, short of a second packet"
        );

        /// The kernel's first `read` came back 1020 bytes: under the old
        /// 1024 threshold, over a keystroke.
        struct ShortOf1024 {
            fd: i32,
            served: bool,
        }
        impl Read for ShortOf1024 {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.served {
                    self.served = true;
                    let n = 1020.min(buf.len());
                    buf[..n].fill(b'A');
                    return Ok(n);
                }
                let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }
        impl Drop for ShortOf1024 {
            fn drop(&mut self) {
                if self.fd >= 0 {
                    unsafe { libc::close(self.fd) };
                    self.fd = -1;
                }
            }
        }

        let poll = unsafe { libc::dup(master) };
        assert!(poll >= 0);
        let mut burst = BurstRead {
            inner: Box::new(ShortOf1024 {
                fd: master,
                served: false,
            }),
            poll_fd: Some(poll),
        };
        let slave = OwnedFd(slave);
        let mut buf = vec![0u8; READ_BURST];
        let n = burst
            .read(&mut buf)
            .expect("drain returns the packet already in hand");
        drop(burst);
        drop(slave);
        assert!(
            n > 1020,
            "a 1020-byte packet must drain the bytes already queued, got {n}"
        );
        assert!(buf[..1020].iter().all(|b| *b == b'A'));
        assert!(
            buf[1020..n].iter().all(|b| *b == b'Q'),
            "drained bytes should be the queued payload"
        );
        assert_eq!(n, 1020 + written);
    }

    /// Cooked `cat` of 80-column lines: default termios, `OPOST|ONLCR`. One
    /// `BurstRead` has to come back with many packets, not the first ~1 KiB.
    #[cfg(unix)]
    #[test]
    fn burst_read_coalesces_onlcr_eighty_column_lines() {
        struct OwnedFd {
            fd: i32,
            sizes: Arc<Mutex<Vec<usize>>>,
        }
        impl Drop for OwnedFd {
            fn drop(&mut self) {
                if self.fd >= 0 {
                    unsafe { libc::close(self.fd) };
                    self.fd = -1;
                }
            }
        }
        impl Read for OwnedFd {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    self.sizes.lock().unwrap().push(n as usize);
                    // The slave refills the ~1 KiB queue after this packet.
                    // Give it the cpu before `drain_queued` polls.
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(50) && !pty_readable(self.fd) {
                        std::thread::yield_now();
                    }
                }
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
        let oflag = termios.c_oflag;
        assert!(
            oflag & libc::OPOST != 0 && oflag & libc::ONLCR != 0,
            "openpty default oflag={oflag:#x}, wanted OPOST|ONLCR"
        );

        let lines = 600;
        let mut input = Vec::with_capacity(lines * 81);
        let mut expected = Vec::with_capacity(lines * 82);
        for _ in 0..lines {
            input.extend(std::iter::repeat_n(b'C', 80));
            input.push(b'\n');
            expected.extend(std::iter::repeat_n(b'C', 80));
            expected.extend_from_slice(b"\r\n");
        }

        let slave_fd = slave;
        let writer = std::thread::spawn(move || {
            let mut off = 0;
            while off < input.len() {
                let end = (off + 8192).min(input.len());
                let n =
                    unsafe { libc::write(slave_fd, input[off..end].as_ptr().cast(), end - off) };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if n == 0 {
                    break;
                }
                off += n as usize;
            }
            unsafe { libc::close(slave_fd) };
            Ok(off)
        });

        let sizes = Arc::new(Mutex::new(Vec::new()));
        let poll = unsafe { libc::dup(master) };
        assert!(poll >= 0);
        let mut burst = BurstRead {
            inner: Box::new(OwnedFd {
                fd: master,
                sizes: Arc::clone(&sizes),
            }),
            poll_fd: Some(poll),
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_BURST];
            let n = burst.read(&mut buf);
            drop(burst);
            let _ = tx.send(n.map(|n| buf[..n].to_vec()));
        });
        let reported = rx.recv_timeout(Duration::from_secs(3));
        let wrote = writer.join().expect("writer thread");
        match reported {
            Ok(Ok(got)) => {
                let wrote = wrote.expect("slave write");
                assert_eq!(wrote, lines * 81);
                assert!(
                    got.len() > 16 * 1024,
                    "one ONLCR read coalesced only {} bytes (kernel reads {:?})",
                    got.len(),
                    sizes.lock().unwrap()
                );
                assert!(
                    expected.starts_with(&got),
                    "coalesced bytes are not the ONLCR expansion of 80-column lines"
                );
                let sizes = sizes.lock().unwrap();
                assert!(
                    sizes.iter().any(|n| *n >= PTY_CHUNK),
                    "drain never engaged; kernel reads were {sizes:?}"
                );
            }
            other => panic!("ONLCR coalesced read did not return: {other:?}"),
        }
    }

    /// Lines whose `ONLCR` expansion is one 1024-byte clist block, read across
    /// several `BurstRead` rounds. The blocking read of each round used to ask
    /// for the whole burst buffer, and Darwin then inserts an extra `\r` where
    /// that block ends on a newline.
    #[cfg(unix)]
    #[test]
    fn burst_read_rounds_over_1024_aligned_newlines_do_not_insert_cr() {
        struct RecordingFd {
            fd: i32,
            asks: Arc<Mutex<Vec<usize>>>,
        }
        impl Drop for RecordingFd {
            fn drop(&mut self) {
                if self.fd >= 0 {
                    unsafe { libc::close(self.fd) };
                    self.fd = -1;
                }
            }
        }
        impl Read for RecordingFd {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.asks.lock().unwrap().push(buf.len());
                let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
        }

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty: {}",
            std::io::Error::last_os_error()
        );
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut termios) }, 0);
        let oflag = termios.c_oflag;
        assert!(
            oflag & libc::OPOST != 0 && oflag & libc::ONLCR != 0,
            "openpty default oflag={oflag:#x}, wanted OPOST|ONLCR"
        );

        // 1022 columns + `\n` expands to 1022 + `\r\n`: one clist block that
        // ends on the newline. Six of them are three rounds of a 2048-byte
        // burst, so each round's blocking read and the drain after it both
        // cross a 1024-byte boundary.
        let lines = 6;
        let mut input = Vec::with_capacity(lines * 1023);
        let mut expected = Vec::with_capacity(lines * 1024);
        for _ in 0..lines {
            input.extend(std::iter::repeat_n(b'L', 1022));
            input.push(b'\n');
            expected.extend(std::iter::repeat_n(b'L', 1022));
            expected.extend_from_slice(b"\r\n");
        }

        let slave_fd = slave;
        let writer = std::thread::spawn(move || {
            let mut off = 0;
            while off < input.len() {
                let n = unsafe {
                    libc::write(slave_fd, input[off..].as_ptr().cast(), input.len() - off)
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if n == 0 {
                    break;
                }
                off += n as usize;
            }
            unsafe { libc::close(slave_fd) };
            Ok(off)
        });

        let asks = Arc::new(Mutex::new(Vec::new()));
        let poll = unsafe { libc::dup(master) };
        assert!(poll >= 0);
        let mut burst = BurstRead {
            inner: Box::new(RecordingFd {
                fd: master,
                asks: Arc::clone(&asks),
            }),
            poll_fd: Some(poll),
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let mut got = Vec::new();
            let result = loop {
                match burst.read(&mut buf) {
                    Ok(0) => break Ok(got),
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    // The slave is closed. Linux reports that as EIO on the
                    // master; macOS reports it as a zero-length read.
                    Err(err) if err.raw_os_error() == Some(libc::EIO) => break Ok(got),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => break Err(err),
                }
            };
            drop(burst);
            let _ = tx.send(result);
        });
        let reported = rx.recv_timeout(Duration::from_secs(3));
        let wrote = writer.join().expect("writer thread");
        match reported {
            Ok(Ok(got)) => {
                assert_eq!(wrote.expect("slave write"), lines * 1023);
                assert_eq!(
                    got, expected,
                    "ONLCR inserted extra bytes across 1024-aligned newlines"
                );
                assert!(
                    !got.windows(2).any(|w| w == b"\r\r"),
                    "a doubled CR survived the aligned rounds"
                );
                let asks = asks.lock().unwrap();
                assert!(
                    asks.len() > 2,
                    "expected several reads across rounds, got {asks:?}"
                );
                #[cfg(target_vendor = "apple")]
                assert!(
                    asks.iter().all(|n| *n <= 1024),
                    "a Darwin read asked for more than one clist block: {asks:?}"
                );
            }
            other => panic!("aligned ONLCR rounds did not return: {other:?}"),
        }
    }

    #[test]
    fn osc_133_marks_b_c_and_d_force_a_group_check() {
        let mut sniffer = OscSniffer::new();
        let began = sniffer.feed(b"\x1b]133;B\x07");
        assert!(began.boundary);
        assert!(!began.command_started);
        let started = sniffer.feed(b"\x1b]133;C;ls\x07");
        assert!(started.boundary && started.command_started);
        let done = sniffer.feed(b"\x1b]133;D;0\x07");
        assert!(done.boundary);
        assert!(!done.command_started);
        let prompt_a = sniffer.feed(b"\x1b]133;A\x07");
        assert!(!prompt_a.boundary);
        assert!(!prompt_a.command_started);
    }

    /// An ESC sequence split across two reads, where the second read contains
    /// no `0x1b`: the rest of a kitty APC is the `\` of an ST, and the rest of
    /// an OSC is payload plus BEL. Skipping those chunks because `memchr`
    /// finds no ESC would drop the image and the command. Mode splits are
    /// covered in `core/term_modes.rs`.
    #[test]
    fn a_sequence_split_across_reads_parses_when_the_second_chunk_has_no_esc() {
        use crate::core::kitty_graphics::Image;
        use base64::Engine as _;

        let pixel = [0xffu8, 0x00, 0x00, 0xff];
        let b64 = base64::engine::general_purpose::STANDARD.encode(pixel);
        let seq = format!("\x1b_Ga=T,f=32,t=d,s=1,v=1,i=1;{b64}\x1b\\");
        let seq = seq.into_bytes();
        assert_eq!(seq[seq.len() - 2], 0x1b);
        let chunk1 = &seq[..seq.len() - 1];
        let mut chunk2 = vec![b'\\'];
        chunk2.extend_from_slice(b"after");
        assert!(crate::core::osc::contains_esc(chunk1));
        assert!(!crate::core::osc::contains_esc(&chunk2));

        let writer = PtyWriter::spawn(Box::new(std::io::sink()));
        let mut clipboard = ClipboardSniffer::default();
        let mut graphics = GraphicsSniffer::new_local(true);
        let mut sniffer = OscSniffer::new();

        let (pass, frames, _) =
            scan_chunk(chunk1, &mut clipboard, &mut graphics, &mut sniffer, &writer);
        assert!(pass.is_empty(), "the APC is not passthrough");
        assert!(frames.is_empty(), "the ST has not arrived");
        assert!(
            !graphics.is_ground(),
            "the fast path must not skip the next chunk"
        );

        let (pass, frames, _) = scan_chunk(
            &chunk2,
            &mut clipboard,
            &mut graphics,
            &mut sniffer,
            &writer,
        );
        assert_eq!(&pass[..], b"after");
        assert!(graphics.is_ground());
        match frames.as_slice() {
            [GraphicsFrame::Image(frame), GraphicsFrame::Output(text)] => {
                let mut img = Image::decode_frame_owned(frame.to_vec()).expect("image");
                assert_eq!(img.id, 1);
                assert_eq!(img.take_rgba8().unwrap(), pixel);
                assert_eq!(text, b"after");
            }
            other => panic!("image then the text after it, got {other:?}"),
        }

        let (pass, frames, signals) = scan_chunk(
            b"plain",
            &mut clipboard,
            &mut graphics,
            &mut sniffer,
            &writer,
        );
        assert_eq!(&pass[..], b"plain");
        assert!(frames.is_empty());
        assert!(!signals.boundary);
        assert!(clipboard.is_ground() && graphics.is_ground());
        assert!(sniffer.is_ground());

        let osc_head = b"\x1b]133;C;echo";
        let osc_tail = b" hi\x07";
        assert!(!crate::core::osc::contains_esc(osc_tail));
        let (_, _, signals) = scan_chunk(
            osc_head,
            &mut clipboard,
            &mut graphics,
            &mut sniffer,
            &writer,
        );
        assert!(!signals.command_started);
        assert!(!sniffer.is_ground());
        let (_, _, signals) = scan_chunk(
            osc_tail,
            &mut clipboard,
            &mut graphics,
            &mut sniffer,
            &writer,
        );
        assert!(signals.command_started);
        assert_eq!(signals.shell[0].command.as_deref(), Some("echo hi"));
        assert!(sniffer.is_ground());
    }

    /// The same splits, through the reader thread: the second chunk of each
    /// sequence has no `0x1b`, and the image and the command still land.
    #[test]
    fn the_reader_parses_sequences_split_across_chunks_with_no_esc_in_the_second() {
        use crate::core::kitty_graphics::Image;
        use base64::Engine as _;

        struct Chunks(std::collections::VecDeque<Vec<u8>>);

        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(&chunk);
                        Ok(chunk.len())
                    }
                    None => Ok(0),
                }
            }
        }

        let pixel = [0xffu8, 0x00, 0x00, 0xff];
        let b64 = base64::engine::general_purpose::STANDARD.encode(pixel);
        let seq = format!("\x1b_Ga=T,f=32,t=d,s=1,v=1,i=1;{b64}\x1b\\");
        let seq = seq.into_bytes();
        let mut kitty_tail = vec![b'\\'];
        kitty_tail.extend_from_slice(b"after");
        assert!(!crate::core::osc::contains_esc(&kitty_tail));
        assert!(!crate::core::osc::contains_esc(b" hi\x07"));
        assert!(!crate::core::osc::contains_esc(b"h"));

        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = output_pipe();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(Chunks(
                [
                    seq[..seq.len() - 1].to_vec(),
                    kitty_tail,
                    b"\x1b]133;C;echo".to_vec(),
                    b" hi\x07".to_vec(),
                    b"\x1b[?1049".to_vec(),
                    b"h".to_vec(),
                ]
                .into_iter()
                .collect(),
            )),
            null_writer(),
            || false,
            ForegroundProbes {
                group: Box::new(|| None),
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        let msgs: Vec<DaemonMsg> = sub_rx.try_iter().collect();
        let image_at = msgs
            .iter()
            .position(|msg| matches!(msg, DaemonMsg::Image(_)))
            .expect("image");
        let after_at = msgs
            .iter()
            .position(|msg| matches!(msg, DaemonMsg::Output(b) if b.starts_with(b"after")))
            .expect("text after the image");
        assert!(
            image_at < after_at,
            "the image is anchored before the text that follows it"
        );
        match &msgs[image_at] {
            DaemonMsg::Image(frame) => {
                let mut img = Image::decode_frame_owned(frame.to_vec()).expect("image");
                assert_eq!(img.take_rgba8().unwrap(), pixel);
            }
            _ => unreachable!(),
        }
        let st = state.lock().unwrap();
        assert_eq!(st.shell.command.as_deref(), Some("echo hi"));
    }

    /// Adjacent output is appended into the `Output` already queued. A `Cwd`,
    /// an `Image`, and an `Exited` stay in the order they were sent, and a
    /// buffer at the cap starts a new `Output` rather than growing without bound.
    #[test]
    fn adjacent_output_coalesces_in_place_and_keeps_control_events_in_order() {
        let (tx, rx) = output_pipe();
        assert!(tx.append(b""));
        assert!(rx.try_recv().is_err(), "an empty append queues nothing");
        assert!(tx.append(b"aaa"));
        assert!(tx.append(b"bbb"));
        assert!(tx.push(DaemonMsg::Cwd(PathBuf::from("/tmp"))));
        assert!(tx.push(DaemonMsg::Output(b"ccc".to_vec())));
        assert!(tx.push(DaemonMsg::Image(b"img".to_vec())));
        assert!(tx.append(b"ddd"));
        assert!(tx.push(DaemonMsg::Exited { code: Some(0) }));

        match rx.try_recv() {
            Ok(DaemonMsg::Output(bytes)) => assert_eq!(bytes, b"aaabbb"),
            other => panic!("coalesced output, got {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(path)) if path == Path::new("/tmp")));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Output(bytes)) if bytes == b"ccc"));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Image(bytes)) if bytes == b"img"));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Output(bytes)) if bytes == b"ddd"));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Exited { code: Some(0) })
        ));
        drop(tx);
        assert!(rx.try_recv().is_err());

        let (tx, rx) = output_pipe();
        let big = vec![b'x'; OUTPUT_COALESCE_MAX];
        assert!(tx.append(&big));
        assert!(tx.append(b"y"));
        drop(tx);
        match rx.try_recv() {
            Ok(DaemonMsg::Output(bytes)) => assert_eq!(bytes.len(), OUTPUT_COALESCE_MAX),
            other => panic!("full buffer, got {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Output(bytes)) if bytes == b"y"));
    }
}
