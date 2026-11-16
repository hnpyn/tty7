#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
mod core;
mod daemon;
mod terminal;
mod ui;

use crate::core::config::Config;
use crate::ui::assets::Assets;
use crate::ui::keymap;
use gpui::*;

fn register_bundled_fonts(cx: &mut App) {
    use std::borrow::Cow;
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-Bold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-Italic.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-BoldItalic.ttf").as_slice()),
    ];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        log::warn!("failed to register bundled Hack fonts: {e}");
    }
}

fn spawn_config_watcher(cx: &mut App) {
    use notify::{RecursiveMode, Watcher};

    let Some(config_file) = crate::core::config::config_path("config.json") else {
        return;
    };
    let Some(dir) = crate::core::config::config_dir_path() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    // FSEvents reports the resolved path, and a watch registered through a
    // symlink (`/tmp` is `/private/tmp` on macOS) never matches it: every
    // edit to config.json went unseen.
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

    let (tx, rx) = smol::channel::unbounded::<()>();
    let watched_file = config_file.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // A reload re-reads the file, and on Linux reading it is itself an
        // event — see `tty7_core::host::is_content_change`.
        if !tty7_core::host::is_content_change(&event.kind) {
            return;
        }
        let hit = event
            .paths
            .iter()
            .any(|p| p.file_name() == watched_file.file_name() || is_theme_file(p));
        if hit {
            let _ = tx.try_send(());
        }
    };

    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("config hot-reload disabled: failed to create watcher: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
        log::warn!(
            "config hot-reload disabled: failed to watch {}: {e}",
            dir.display()
        );
        return;
    }
    Box::leak(Box::new(watcher));

    cx.spawn(async move |cx| {
        // One toast per breakage rather than one per write: this watcher also
        // fires for every theme file, so a config.json left broken would
        // otherwise re-announce itself on each of them. Cleared by the load
        // that parses, so a second breakage speaks up again.
        let mut announced = false;
        while rx.recv().await.is_ok() {
            cx.background_executor().timer(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}

            cx.update(|cx| {
                apply_reloaded_config(cx, Config::load_with_outcome(), &mut announced);
            });
        }
    })
    .detach();
}

/// Applies one debounced config reload. Returns whether the keymap was rebuilt.
fn apply_reloaded_config(
    cx: &mut App,
    load: (Config, crate::core::config::LoadOutcome),
    announced: &mut bool,
) -> bool {
    let (config, outcome) = load;
    if outcome.failed() {
        // Keep the running config. Swapping in defaults would flash the UI, and the broken file is already parked.
        if !*announced {
            *announced = true;
            notify_config_load_failed(cx, outcome, false);
        }
        // Theme files share this watcher and must still reload while config.json is broken.
        reload_themes(cx);
        cx.refresh_windows();
        return false;
    }
    *announced = false;
    // Rebuild bindings only when one actually moved. This fires on every config write.
    let keymap_before = crate::ui::keymap::keybinding_config(cx);
    crate::ui::i18n::set_locale(&config.gui_language);
    cx.set_global(config);
    reload_themes(cx);
    crate::ui::theme::apply_cursor_hide_mode(cx);
    let rebound = crate::ui::keymap::keybinding_config(cx) != keymap_before;
    if rebound {
        crate::ui::keymap::rebind(cx);
    }
    cx.refresh_windows();
    rebound
}

fn is_theme_file(p: &std::path::Path) -> bool {
    p.parent().and_then(|d| d.file_name()) == Some(std::ffi::OsStr::new("themes"))
        && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            e.eq_ignore_ascii_case("yaml")
                || e.eq_ignore_ascii_case("yml")
                || e.eq_ignore_ascii_case("itermcolors")
        })
}

/// Re-reads theme files. Runs even when config.json failed, so a typo there cannot freeze theme edits.
fn reload_themes(cx: &mut App) {
    crate::ui::presets::load_registry(cx);
    crate::ui::theme::apply_theme(None, cx);
}

/// Reports that config.json did not load, and where its contents were parked.
fn notify_config_load_failed(
    _cx: &mut App,
    outcome: crate::core::config::LoadOutcome,
    startup: bool,
) {
    log::warn!("config load failed ({outcome:?}, startup={startup})");
}

fn strip_os_arg_prefix(arg: &std::ffi::OsStr, prefix: &str) -> Option<std::ffi::OsString> {
    let suffix = arg.as_encoded_bytes().strip_prefix(prefix.as_bytes())?;
    // SAFETY: `prefix` is ASCII and is removed only from the beginning of an
    // existing platform-encoded OsStr. ASCII bytes are self-synchronizing in
    // Windows WTF-8 and Unix byte strings, so the suffix keeps valid encoding.
    Some(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(suffix.to_vec()) })
}

fn config_dir_from(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    while let Some(arg) = args.next() {
        if let Some(path) = strip_os_arg_prefix(&arg, "--config-dir=") {
            return Some(path.into());
        }
        if arg == std::ffi::OsStr::new("--config-dir") {
            return args.next().map(Into::into);
        }
    }
    None
}

fn apply_config_dir_arg(args: &[std::ffi::OsString]) {
    if let Some(path) = config_dir_from(args.iter().cloned()) {
        crate::core::config::set_config_dir(path);
    }
}

fn nonempty_path(path: std::path::PathBuf) -> Option<std::path::PathBuf> {
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

fn open_path_from(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    let mut positional = None;
    while let Some(arg) = args.next() {
        if let Some(path) = strip_os_arg_prefix(&arg, "--open-path=") {
            return nonempty_path(path.into());
        }
        if arg == std::ffi::OsStr::new("--open-path") {
            return args
                .next()
                .map(std::path::PathBuf::from)
                .and_then(nonempty_path);
        }
        // `--config-dir <path>` and `--display <id>` are values, not the
        // directory to open.
        if arg == std::ffi::OsStr::new("--config-dir") || arg == std::ffi::OsStr::new("--display") {
            let _ = args.next();
            continue;
        }
        if arg.as_encoded_bytes().first() == Some(&b'-') {
            continue;
        }
        if positional.is_none() {
            positional = nonempty_path(std::path::PathBuf::from(arg));
        }
    }
    positional
}

/// A path from `open -a` / Finder: a `file://` URL, or a plain path.
pub(crate) fn path_from_open_target(raw: &str) -> Option<std::path::PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('-') {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("file://") {
        return file_url_to_path(rest);
    }
    Some(std::path::PathBuf::from(raw))
}

fn file_url_to_path(rest: &str) -> Option<std::path::PathBuf> {
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(rest);
    // `file:///C:/dir` arrives as `/C:/dir`. A leading slash in front of a
    // drive letter is not part of the path.
    let path = match decoded.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => decoded[1..].to_string(),
        _ => decoded,
    };
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A dropped file opens its parent directory. A directory opens itself.
///
/// Relative paths, including a bare file name whose `parent()` is empty, are
/// resolved against the process's current directory first. `~` is the user's
/// home. `.` components are dropped so `ntty7 .` is that directory itself.
pub(crate) fn directory_for_open(path: &std::path::Path) -> std::path::PathBuf {
    let path = expand_home(path);
    let anchored = if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(&path),
            Err(_) => path,
        }
    };
    let anchored = normalize_dots(&anchored);
    if anchored.is_dir() {
        return anchored;
    }
    match anchored.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => anchored,
    }
}

fn expand_home(path: &std::path::Path) -> std::path::PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = text.strip_prefix('~') else {
        return path.to_path_buf();
    };
    if !rest.is_empty() && !rest.starts_with('/') && !rest.starts_with('\\') {
        return path.to_path_buf();
    }
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return path.to_path_buf();
    };
    let mut out = std::path::PathBuf::from(home);
    let rest = rest.trim_start_matches(['/', '\\']);
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

fn normalize_dots(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

static PENDING_OPEN: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
static WINDOW_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static OPENED_PATH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// How long after a path-less cold launch the first Finder / `open -a` batch
/// may replace the window instead of starting a second process. The URL
/// arrives on the main loop just after `applicationDidFinishLaunching`.
pub(crate) const LAUNCH_URL_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

struct ColdLaunch {
    opened_at: std::time::Instant,
    explicit_path: bool,
    absorbed: std::sync::atomic::AtomicBool,
}

static COLD_LAUNCH: std::sync::OnceLock<ColdLaunch> = std::sync::OnceLock::new();
static LAUNCH_RETARGET: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// The first open-URL batch after a cold launch with no path replaces the
/// window that already came up in `$HOME`. A later drop, a window that was
/// opened on a path, or a second batch starts another process.
pub(crate) fn absorb_cold_open(
    window_live: bool,
    explicit_path: bool,
    absorbed: bool,
    age: std::time::Duration,
) -> bool {
    window_live && !explicit_path && !absorbed && age < LAUNCH_URL_GRACE
}

fn window_is_live() -> bool {
    WINDOW_LIVE.load(std::sync::atomic::Ordering::SeqCst)
}

fn opened_here(path: &std::path::Path) -> bool {
    OPENED_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .is_some_and(|opened| opened == path)
}

pub(crate) fn mark_window_live(cwd: Option<&std::path::Path>) {
    let _ = COLD_LAUNCH.set(ColdLaunch {
        opened_at: std::time::Instant::now(),
        explicit_path: cwd.is_some(),
        absorbed: std::sync::atomic::AtomicBool::new(false),
    });
    if let Some(cwd) = cwd {
        *OPENED_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(cwd.to_path_buf());
    }
    WINDOW_LIVE.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn take_pending_open() -> Option<std::path::PathBuf> {
    PENDING_OPEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// The directory a path-less launch window should switch to, if a cold-start
/// open URL arrived. The window polls this; `on_open_urls` has no `App`.
pub(crate) fn take_launch_retarget() -> Option<std::path::PathBuf> {
    LAUNCH_RETARGET
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

pub(crate) fn within_launch_grace() -> bool {
    COLD_LAUNCH
        .get()
        .is_some_and(|launch| launch.opened_at.elapsed() < LAUNCH_URL_GRACE)
}

/// Finder and `open -a` deliver paths here. The first one, before the window
/// exists, is this process's directory. The first batch in the launch grace,
/// when that window had no path, replaces it. Anything later is another
/// window, which is another process.
fn note_external_opens(urls: Vec<String>) {
    let mut dirs = Vec::new();
    for url in urls {
        let Some(path) = path_from_open_target(&url) else {
            continue;
        };
        let dir = directory_for_open(&path);
        if dirs.iter().any(|have: &std::path::PathBuf| have == &dir) || opened_here(&dir) {
            continue;
        }
        dirs.push(dir);
    }
    if dirs.is_empty() {
        return;
    }
    if let Some(launch) = COLD_LAUNCH.get()
        && absorb_cold_open(
            window_is_live(),
            launch.explicit_path,
            launch.absorbed.load(std::sync::atomic::Ordering::SeqCst),
            launch.opened_at.elapsed(),
        )
    {
        launch
            .absorbed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut dirs = dirs.into_iter();
        if let Some(first) = dirs.next() {
            *LAUNCH_RETARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(first.clone());
            *OPENED_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(first);
        }
        for extra in dirs {
            spawn_new_instance(Some(&extra), None);
        }
        return;
    }
    if window_is_live() {
        for dir in dirs {
            spawn_new_instance(Some(&dir), None);
        }
        return;
    }
    let mut pending = PENDING_OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let mut dirs = dirs.into_iter();
    if pending.is_none() {
        *pending = dirs.next();
    }
    drop(pending);
    for dir in dirs {
        spawn_new_instance(Some(&dir), None);
    }
}

#[cfg(unix)]
fn merge_paths(primary: &str, secondary: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    primary
        .split(':')
        .chain(secondary.split(':'))
        .filter(|p| !p.is_empty() && seen.insert(*p))
        .collect::<Vec<_>>()
        .join(":")
}

/// Brackets the PATH in the probe's output. An interactive rc prints what it
/// likes — a greeting above, an exit hook below — so neither the whole output
/// nor any one line of it can be trusted to be the PATH.
#[cfg(unix)]
const PATH_MARKER: &str = "__TTY7_PATH__";

/// How long startup waits on the user's rc files before giving up on them.
#[cfg(unix)]
const PATH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The PATH between the first pair of markers, or `None` until both are in.
#[cfg(unix)]
fn path_from_shell_output(out: &[u8]) -> Option<String> {
    let out = String::from_utf8_lossy(out);
    let (_, rest) = out.split_once(PATH_MARKER)?;
    let (path, _) = rest.split_once(PATH_MARKER)?;
    Some(path.trim().to_string())
}

/// Runs the probe and returns the PATH as soon as the closing marker arrives,
/// without waiting for EOF: an rc that backgrounds a daemon hands it our
/// stdout, and the pipe then never closes. Past the timeout the shell is
/// killed and startup goes on with the PATH it has.
#[cfg(unix)]
fn read_path_from_shell(shell: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = match Command::new(shell)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log::warn!("failed to spawn login shell {shell} for PATH: {e}");
            return None;
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.wait();
            return None;
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
            if let Some(path) = path_from_shell_output(&out) {
                let _ = tx.send(Some(path));
                return;
            }
        }
        let _ = tx.send(None);
    });
    let path = match rx.recv_timeout(PATH_PROBE_TIMEOUT) {
        Ok(path) => path,
        Err(_) => {
            log::warn!(
                "login shell {shell} did not report PATH within {PATH_PROBE_TIMEOUT:?}; \
                 continuing without it"
            );
            None
        }
    };
    // Kill, join, then reap before returning. `set_var` is single-threaded,
    // and a reader or reaper still alive here would be a second thread in
    // `setenv`'s critical section. Killing also unblocks a read parked on a
    // shell whose exit hooks never close the pipe.
    let _ = child.kill();
    let _ = reader.join();
    let _ = child.wait();
    path
}

#[cfg(unix)]
fn enrich_path_from_login_shell() {
    let shell = crate::core::shells::login_shell();
    let fish = std::path::Path::new(&shell).file_name() == Some("fish".as_ref());
    let cmd = if fish {
        format!("printf '{PATH_MARKER}%s{PATH_MARKER}' (string join ':' $PATH)")
    } else {
        format!("printf '{PATH_MARKER}%s{PATH_MARKER}' \"$PATH\"")
    };
    // A pane gets an interactive shell, and for zsh/bash that is the only one
    // that reads `.zshrc` / `.bashrc` — where most people put their PATH. A
    // login shell alone reads `.zprofile` / `.bash_profile` and quietly hands
    // back the GUI's own bare PATH, so half the tools on `PATH` in a terminal
    // do not exist as far as this app is concerned. fish reads its config in
    // every mode; `-i` without a tty only makes it complain on stderr.
    let args: Vec<&str> = if fish {
        vec!["-l", "-c", &cmd]
    } else {
        vec!["-i", "-l", "-c", &cmd]
    };
    let Some(login_path) = read_path_from_shell(&shell, &args) else {
        return;
    };
    if login_path.is_empty() {
        return;
    }
    let merged = merge_paths(&login_path, &std::env::var("PATH").unwrap_or_default());
    unsafe { std::env::set_var("PATH", merged) };
}

#[cfg(target_os = "macos")]
fn set_dock_icon_for_bare_binary() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let bundled = std::env::current_exe().is_ok_and(|p| {
        p.components()
            .any(|c| c.as_os_str().to_string_lossy().ends_with(".app"))
    });
    if bundled {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    static ICON_PNG: &[u8] = include_bytes!("../assets/app-icon.png");
    let data = NSData::with_bytes(ICON_PNG);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe {
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
        }
    }
}

/// Turns gpui's stroke thickening off for tty7's text when `font_thicken` is
/// off; with it on, whatever the system (or a hand-written `defaults write`)
/// says stands.
///
/// The switch lives in gpui's rasterizer and nowhere else. Pinning
/// `AppleFontSmoothing` to `0` for the process (which is how this used to be
/// done) reached AppKit as well: the menus, alerts and every other natively
/// drawn string lost their smoothing, while the terminal's dark-on-light text,
/// which gpui never thickens, looked no different (#1119).
///
/// Applied once at launch, before any text is drawn; a change made in Settings
/// waits for the next one.
#[cfg(target_os = "macos")]
fn apply_font_thicken(thicken: bool) {
    gpui_platform::set_glyph_dilation(thicken);
}

/// Drop the identity a parent terminal exported before this process reads
/// config or spawns a shell.
///
/// `tty7` publishes `TTY7_*` (config dir, pane, workspace, shell integration,
/// and a hijacked `ZDOTDIR`). A parent `ntty7` pane publishes the same facts
/// under `NTTY7_*`. Either set would make this window answer as that pane, or
/// write into the other app's directory. A user-chosen `$NTTY7_CONFIG_DIR`
/// stays; one whose directory name is `ntty7-<pid>` is the parent's ephemeral
/// runtime and is not a config directory.
/// Nested-pane markers. A blanket `NTTY7_*` wipe would also drop a
/// user-chosen `NTTY7_CONFIG_DIR` and `NTTY7_TRACE`.
const NESTED_PANE_ENV: &[&str] = &[
    "NTTY7",
    "NTTY7_USER_ZDOTDIR",
    "NTTY7_RC",
    "NTTY7_ZDOTDIR",
    "NTTY7_PANE",
    "NTTY7_WS",
    "NTTY7_SHELL_INTEGRATION",
    "NTTY7_HISTFILE",
];

fn scrub_inherited_terminal_env() {
    restore_zdotdir_from_outer_tty7();
    let inherited: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            let name = key.to_string_lossy();
            name == "TTY7" || name.starts_with("TTY7_")
        })
        .collect();
    for key in inherited {
        unsafe { std::env::remove_var(key) };
    }
    for key in NESTED_PANE_ENV {
        unsafe { std::env::remove_var(key) };
    }
    if let Some(value) = std::env::var_os("NTTY7_CONFIG_DIR") {
        let name = std::path::Path::new(&value)
            .file_name()
            .and_then(|name| name.to_str());
        if inherited_runtime_dirname_is_ephemeral(name) {
            unsafe { std::env::remove_var("NTTY7_CONFIG_DIR") };
        }
    }
}

/// `tty7` and `ntty7` point `ZDOTDIR` at a throwaway directory for the
/// duration of zsh startup and keep the user's directory in
/// `TTY7_USER_ZDOTDIR` / `NTTY7_USER_ZDOTDIR`. Copy that back, then drop both
/// variables. A `ZDOTDIR` that is not one of those throwaways is the user's
/// own — `export ZDOTDIR=/custom` inside a nested shell — and is left alone
/// even when a stale save is still set.
fn restore_zdotdir_from_outer_tty7() {
    let current = std::env::var_os("ZDOTDIR");
    let tty7_saved = std::env::var_os("TTY7_USER_ZDOTDIR");
    let ntty7_saved = std::env::var_os("NTTY7_USER_ZDOTDIR");
    match plan_inherited_zdotdir(
        current.as_deref(),
        tty7_saved.as_deref(),
        ntty7_saved.as_deref(),
    ) {
        ZdotdirRestore::Leave => {}
        ZdotdirRestore::Clear => unsafe { std::env::remove_var("ZDOTDIR") },
        ZdotdirRestore::Set(path) => unsafe { std::env::set_var("ZDOTDIR", path) },
    }
    unsafe { std::env::remove_var("TTY7_USER_ZDOTDIR") };
    unsafe { std::env::remove_var("NTTY7_USER_ZDOTDIR") };
}

enum ZdotdirRestore {
    Leave,
    Clear,
    Set(std::ffi::OsString),
}

fn zdotdir_restore(saved: Option<&std::ffi::OsStr>) -> ZdotdirRestore {
    match saved {
        None => ZdotdirRestore::Leave,
        Some(value) if value.is_empty() || zdotdir_is_throwaway(value) => ZdotdirRestore::Clear,
        Some(value) => ZdotdirRestore::Set(value.to_os_string()),
    }
}

/// What to do with the `ZDOTDIR` inherited from an outer terminal.
///
/// A current directory that is not a `tty7-zdotdir-*` / `ntty7-zdotdir-*`
/// throwaway is kept. A throwaway is replaced from the matching save
/// (`ntty7-zdotdir-*` prefers `NTTY7_USER_ZDOTDIR`, `tty7-zdotdir-*` prefers
/// `TTY7_USER_ZDOTDIR`). An empty or throwaway save clears `ZDOTDIR`. When
/// `ZDOTDIR` itself is unset, a real save is put back.
fn plan_inherited_zdotdir(
    current: Option<&std::ffi::OsStr>,
    tty7_saved: Option<&std::ffi::OsStr>,
    ntty7_saved: Option<&std::ffi::OsStr>,
) -> ZdotdirRestore {
    if current.is_some() && !current.is_some_and(zdotdir_is_throwaway) {
        return ZdotdirRestore::Leave;
    }
    let saved = preferred_saved_zdotdir(current, tty7_saved, ntty7_saved);
    match zdotdir_restore(saved) {
        ZdotdirRestore::Leave if current.is_some_and(zdotdir_is_throwaway) => ZdotdirRestore::Clear,
        other => other,
    }
}

fn preferred_saved_zdotdir<'a>(
    current: Option<&std::ffi::OsStr>,
    tty7_saved: Option<&'a std::ffi::OsStr>,
    ntty7_saved: Option<&'a std::ffi::OsStr>,
) -> Option<&'a std::ffi::OsStr> {
    let name = current
        .and_then(|path| std::path::Path::new(path).file_name())
        .and_then(|name| name.to_str());
    if name.is_some_and(|name| name.starts_with("ntty7-zdotdir-")) {
        ntty7_saved.or(tty7_saved)
    } else if name.is_some_and(|name| name.starts_with("tty7-zdotdir-")) {
        tty7_saved.or(ntty7_saved)
    } else {
        ntty7_saved.or(tty7_saved)
    }
}

fn zdotdir_is_throwaway(path: &std::ffi::OsStr) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("tty7-zdotdir-") || name.starts_with("ntty7-zdotdir-"))
}

fn strip_inherited_terminal_env(cmd: &mut std::process::Command) {
    let inherited: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            let name = key.to_string_lossy();
            name == "TTY7" || name.starts_with("TTY7_")
        })
        .collect();
    for key in inherited {
        cmd.env_remove(key);
    }
    for key in NESTED_PANE_ENV {
        cmd.env_remove(key);
    }
}

pub(crate) fn inherited_runtime_dirname_is_ephemeral(name: Option<&str>) -> bool {
    let Some(name) = name else {
        return false;
    };
    let Some(pid) = name.strip_prefix("ntty7-") else {
        return false;
    };
    !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())
}

pub(crate) fn cleanup_runtime() {
    // Drop every live pane on this thread. gpui only polls the quit future
    // for 200ms, which is not long enough for a pane's history merge.
    crate::terminal::shutdown_panes();
}

pub(crate) fn cleanup_and_quit(cx: &mut App) {
    cleanup_runtime();
    cx.quit();
}

pub(crate) fn new_instance_args(
    cwd: Option<&std::path::Path>,
    config_dir: Option<&std::path::Path>,
    display: Option<u64>,
) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    if let Some(cwd) = cwd {
        args.push("--open-path".into());
        args.push(cwd.as_os_str().to_os_string());
    }
    if let Some(dir) = config_dir {
        args.push("--config-dir".into());
        args.push(dir.as_os_str().to_os_string());
    }
    if let Some(display) = display {
        args.push("--display".into());
        args.push(display.to_string().into());
    }
    args
}

/// Display the parent window asked this process to open on.
static INHERITED_DISPLAY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub(crate) fn note_inherited_display(id: u64) {
    let _ = INHERITED_DISPLAY.set(id);
}

pub(crate) fn inherited_display() -> Option<u64> {
    INHERITED_DISPLAY.get().copied()
}

pub(crate) fn display_from_args(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<u64> {
    while let Some(arg) = args.next() {
        if let Some(raw) = strip_os_arg_prefix(&arg, "--display=") {
            return parse_display_id(&raw);
        }
        if arg == std::ffi::OsStr::new("--display") {
            let raw = args.next()?;
            return parse_display_id(&raw);
        }
    }
    None
}

fn parse_display_id(raw: &std::ffi::OsStr) -> Option<u64> {
    raw.to_str()?.parse().ok()
}

#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
}

pub(crate) fn spawn_new_instance(cwd: Option<&std::path::Path>, display: Option<u64>) {
    let Ok(exe) = std::env::current_exe() else {
        log::warn!("could not locate ntty7 to open another window");
        return;
    };
    let config = crate::core::config::config_dir_path();
    let mut cmd = std::process::Command::new(exe);
    cmd.args(new_instance_args(cwd, config.as_deref(), display));
    strip_inherited_terminal_env(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    detach(&mut cmd);
    if let Err(error) = cmd.spawn() {
        log::warn!("failed to open a new window: {error}");
    }
}

fn main() {
    // Before args, config, or the log file: a parent tty7's `TTY7_*` must not
    // decide this process's directory.
    scrub_inherited_terminal_env();

    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(id) = display_from_args(args.iter().cloned()) {
        note_inherited_display(id);
    }

    apply_config_dir_arg(&args);
    // A killed window never retires. Drop files whose process is gone; a
    // live window's pane-history stays, because the names carry its pid.
    crate::daemon::history::sweep(&std::collections::HashSet::new());
    crate::core::crash::install("gui");
    crate::core::logfile::install("gui");

    #[cfg(unix)]
    enrich_path_from_login_shell();

    let (config, config_outcome) = crate::core::config::Config::load_with_outcome();
    let gui_language = config.gui_language.clone();
    #[cfg(target_os = "macos")]
    apply_font_thicken(config.font_thicken);

    #[cfg(target_os = "windows")]
    crate::core::aumid::init();

    let open_path = open_path_from(args.into_iter()).map(|path| directory_for_open(&path));
    let application = gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(QuitMode::Explicit);
    application.on_open_urls(note_external_opens);
    application.on_reopen(|cx| {
        cx.spawn(async move |cx| {
            cx.update(|cx| cx.activate(true));
        })
        .detach();
    });
    application.run(move |cx| {
        gpui_component::init(cx);
        register_bundled_fonts(cx);
        crate::ui::prompt::install(cx);
        cx.activate(true);
        #[cfg(target_os = "macos")]
        set_dock_icon_for_bare_binary();
        crate::ui::i18n::set_locale(&gui_language);
        cx.set_global(config);
        crate::ui::theme::refresh_system_appearance(cx);
        crate::ui::presets::load_registry(cx);
        crate::ui::theme::apply_cursor_hide_mode(cx);
        spawn_config_watcher(cx);
        // Dock Quit calls `NSApplication::terminate:` and never returns from
        // `run`. The cleanup has to happen when the observer is invoked: gpui
        // only polls the returned future for 200ms, which is not long enough
        // to drop a pane. The subscription has to outlive `run`.
        std::mem::forget(cx.on_app_quit(|_cx| {
            cleanup_runtime();
            async {}
        }));
        keymap::init(cx);
        crate::ui::solo::open(cx, open_path);
        if config_outcome.failed() {
            notify_config_load_failed(cx, config_outcome, true);
        }
    });
    cleanup_runtime();
}

#[cfg(all(test, target_os = "macos"))]
mod font_thicken_tests {
    use super::apply_font_thicken;
    use objc2_foundation::{NSArgumentDomain, NSUserDefaults, ns_string};

    /// #1119: turning thickening off must not reach AppKit. `AppleFontSmoothing`
    /// in any domain this process reads restyles its native menus and alerts,
    /// so the switch may only ever talk to gpui.
    #[test]
    fn turning_thickening_off_leaves_the_smoothing_preference_alone() {
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = ns_string!("AppleFontSmoothing");
        let before = defaults.objectForKey(key);

        apply_font_thicken(false);

        // SAFETY: a Foundation constant, initialized before `main` runs.
        let arguments = defaults.volatileDomainForName(unsafe { NSArgumentDomain });
        assert!(arguments.objectForKey(key).is_none());
        assert_eq!(defaults.objectForKey(key).is_some(), before.is_some());

        apply_font_thicken(true);
    }
}

/// The watcher tick, from a reloaded file to the keys the app dispatches on.
///
/// The one thing #548 is for — hand-editing config.json and having the new
/// chord fire without a restart — crosses a file watcher, a debounce and a
/// keymap rebuild, and used to be covered nowhere. These drive
/// `apply_reloaded_config` directly, which is the whole body of the watcher's
/// tick, so the reload path is exercised without waiting on the filesystem.
#[cfg(test)]
mod config_reload_tests {
    use super::{apply_reloaded_config, is_theme_file};
    use crate::core::actions::NewWindow;
    use crate::core::config::{Config, LoadOutcome};
    use gpui::{Action as _, App, KeyContext, Keystroke, TestAppContext};

    /// What the live keymap — the one gpui dispatches against — does with
    /// `keys` typed in a terminal.
    fn dispatched(cx: &App, keys: &str) -> Vec<&'static str> {
        let typed = [Keystroke::parse(keys).expect("the typed keystroke parses")];
        let context = [KeyContext::parse("Terminal").expect("the context parses")];
        cx.key_bindings()
            .borrow()
            .bindings_for_input(&typed, &context)
            .0
            .iter()
            .map(|b| b.action().name())
            .collect()
    }

    /// An app running on `config`, as far as the config watcher can tell:
    /// the globals its tick reads, and a keymap built from that config.
    fn running_on(cx: &mut App, config: Config) {
        let dir = std::env::temp_dir().join(format!("tty7-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);

        gpui_component::init(cx);
        cx.set_global(config);
        crate::ui::presets::load_registry(cx);
        crate::ui::keymap::init(cx);
    }

    fn bound_to_new_window(config: &mut Config, key: &str) {
        config.keybindings.insert(
            "NewWindow".to_string(),
            crate::core::config::KeybindingOverride::Add(key.to_string()),
        );
    }

    #[gpui::test]
    fn a_hand_edited_binding_fires_without_a_restart(cx: &mut TestAppContext) {
        cx.update(|cx| {
            running_on(cx, Config::default());
            assert!(
                dispatched(cx, "ctrl-alt-9").is_empty(),
                "nothing is on the chord before the edit"
            );

            let mut edited = Config::default();
            bound_to_new_window(&mut edited, "ctrl-alt-9");
            let mut announced = false;
            assert!(
                apply_reloaded_config(cx, (edited, LoadOutcome::Parsed), &mut announced),
                "a moved binding rebuilds the keymap"
            );

            assert_eq!(
                dispatched(cx, "ctrl-alt-9"),
                vec![NewWindow::name_for_type()],
                "the hand-edited chord reaches the keymap gpui dispatches on"
            );
        });
    }

    #[gpui::test]
    fn a_save_that_moves_no_binding_leaves_the_keymap_alone(cx: &mut TestAppContext) {
        cx.update(|cx| {
            running_on(cx, Config::default());

            // The app's own `save()` fires this watcher on every sidebar drag.
            let mut unrelated = Config::default();
            unrelated.copy_on_select = !unrelated.copy_on_select;
            let mut announced = false;
            assert!(
                !apply_reloaded_config(
                    cx,
                    (unrelated.clone(), LoadOutcome::Parsed),
                    &mut announced
                ),
                "an unrelated save must not rebuild the keymap"
            );
            assert_eq!(
                cx.global::<Config>().copy_on_select,
                unrelated.copy_on_select,
                "the reload still took effect; only the rebind was skipped"
            );
        });
    }

    #[gpui::test]
    fn a_broken_config_does_not_take_the_users_keys_away(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut user = Config::default();
            bound_to_new_window(&mut user, "ctrl-alt-9");
            running_on(cx, user);
            assert_eq!(
                dispatched(cx, "ctrl-alt-9"),
                vec![NewWindow::name_for_type()]
            );

            // A quarantined load hands back stand-in defaults, and the global
            // is deliberately left alone — so nothing may rebind off them.
            let mut announced = false;
            assert!(
                !apply_reloaded_config(
                    cx,
                    (Config::default(), LoadOutcome::Quarantined),
                    &mut announced
                ),
                "a load that failed has nothing to compare and must not rebind"
            );
            assert!(announced, "the breakage is announced");
            assert_eq!(
                cx.global::<Config>().keybindings.get("NewWindow"),
                Some(&crate::core::config::KeybindingOverride::Add(
                    "ctrl-alt-9".to_string()
                )),
                "the running config survives the broken file"
            );
            assert_eq!(
                dispatched(cx, "ctrl-alt-9"),
                vec![NewWindow::name_for_type()],
                "and so do the keys the app is dispatching on"
            );

            // The file parses again: the latch clears, so a second breakage
            // can speak up.
            let mut fixed = Config::default();
            bound_to_new_window(&mut fixed, "ctrl-alt-8");
            assert!(apply_reloaded_config(
                cx,
                (fixed, LoadOutcome::Parsed),
                &mut announced
            ));
            assert!(!announced);
            assert_eq!(
                dispatched(cx, "ctrl-alt-8"),
                vec![NewWindow::name_for_type()]
            );
            assert!(
                dispatched(cx, "ctrl-alt-9").is_empty(),
                "the chord the fixed file dropped is retired"
            );
        });
    }

    /// The watcher covers the whole config directory, and the themes half of
    /// it has to keep reloading even when config.json does not parse.
    #[test]
    fn only_theme_files_beside_the_config_count_as_themes() {
        use std::path::Path;
        assert!(is_theme_file(Path::new("/cfg/themes/solar.yaml")));
        assert!(is_theme_file(Path::new("/cfg/themes/solar.YML")));
        assert!(is_theme_file(Path::new("/cfg/themes/solar.itermcolors")));
        assert!(!is_theme_file(Path::new("/cfg/themes/notes.txt")));
        assert!(!is_theme_file(Path::new("/cfg/config.json")));
        assert!(!is_theme_file(Path::new("/cfg/solar.yaml")));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{merge_paths, path_from_shell_output};

    #[test]
    fn path_from_shell_output_reads_between_the_markers() {
        // The shape `zsh -i` actually prints: an rc that greets the user, the
        // `can't change option: zle` chatter `-i` earns without a tty, and an
        // exit hook that prints after the PATH.
        assert_eq!(
            path_from_shell_output(
                b"hello\n(eval):1: can't change option: zle\n\
                  __TTY7_PATH__/opt/bin:/usr/bin__TTY7_PATH__bye\n"
            )
            .as_deref(),
            Some("/opt/bin:/usr/bin")
        );
        assert_eq!(
            path_from_shell_output(b"hello\n__TTY7_PATH__/opt/bin"),
            None
        );
        assert_eq!(path_from_shell_output(b"/opt/bin:/usr/bin\n"), None);
    }

    #[test]
    fn merge_paths_prefers_primary_dedupes_and_drops_empties() {
        assert_eq!(
            merge_paths("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin:"),
            "/opt/homebrew/bin:/usr/bin:/bin"
        );
        assert_eq!(
            merge_paths(
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
                "/usr/bin:/bin"
            ),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
        );
        assert_eq!(merge_paths("", "/usr/bin"), "/usr/bin");
    }
}

#[cfg(test)]
mod argument_tests {
    use super::{config_dir_from, open_path_from};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn open_path_accepts_separate_and_equals_forms() {
        let separate = open_path_from(
            ["--config-dir", "/cfg", "--open-path", "/work"]
                .into_iter()
                .map(OsString::from),
        );
        assert_eq!(separate, Some(PathBuf::from("/work")));

        let equals = open_path_from([OsString::from("--open-path=C:\\work")].into_iter());
        assert_eq!(equals, Some(PathBuf::from("C:\\work")));
    }

    #[test]
    fn config_dir_accepts_native_separate_and_equals_forms() {
        let separate = config_dir_from(
            [OsString::from("--config-dir"), OsString::from("C:\\cfg")].into_iter(),
        );
        assert_eq!(separate, Some(PathBuf::from("C:\\cfg")));

        let equals = config_dir_from([OsString::from("--config-dir=C:\\cfg")].into_iter());
        assert_eq!(equals, Some(PathBuf::from("C:\\cfg")));
    }

    #[cfg(windows)]
    #[test]
    fn open_path_preserves_unpaired_utf16_from_windows_arguments() {
        use std::os::windows::ffi::OsStringExt as _;

        let native_path =
            OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16]);
        let parsed =
            open_path_from([OsString::from("--open-path"), native_path.clone()].into_iter());
        assert_eq!(parsed, Some(PathBuf::from(native_path.clone())));

        let mut equals_arg = OsString::from("--open-path=");
        equals_arg.push(&native_path);
        assert_eq!(
            open_path_from([equals_arg].into_iter()),
            Some(PathBuf::from(native_path))
        );
    }

    #[test]
    fn new_instance_args_carry_the_cwd_and_the_shared_config_dir() {
        let cwd = std::path::Path::new("/work/app");
        let cfg = std::path::Path::new("/home/me/.config/ntty7");
        assert_eq!(
            super::new_instance_args(Some(cwd), Some(cfg), Some(42)),
            vec![
                std::ffi::OsString::from("--open-path"),
                std::ffi::OsString::from("/work/app"),
                std::ffi::OsString::from("--config-dir"),
                std::ffi::OsString::from("/home/me/.config/ntty7"),
                std::ffi::OsString::from("--display"),
                std::ffi::OsString::from("42"),
            ]
        );
        assert!(super::new_instance_args(None, None, None).is_empty());
    }

    #[test]
    fn a_display_flag_is_not_the_directory_to_open() {
        assert_eq!(
            open_path_from(["--display", "42", "/work"].into_iter().map(OsString::from)),
            Some(PathBuf::from("/work"))
        );
        assert_eq!(
            super::display_from_args(
                ["--display", "42", "--open-path", "/work"]
                    .into_iter()
                    .map(OsString::from)
            ),
            Some(42)
        );
        assert_eq!(
            super::display_from_args([OsString::from("--display=7")].into_iter()),
            Some(7)
        );
        assert_eq!(
            super::display_from_args([OsString::from("--display=nope")].into_iter()),
            None
        );
        assert_eq!(
            super::display_from_args(
                [OsString::from("--open-path"), OsString::from("/work")].into_iter()
            ),
            None
        );
    }

    #[test]
    fn open_path_accepts_a_positional_directory_and_skips_flags() {
        assert_eq!(
            open_path_from([OsString::from("~/proj")].into_iter()),
            Some(PathBuf::from("~/proj"))
        );
        assert_eq!(
            open_path_from([OsString::from(".")].into_iter()),
            Some(PathBuf::from("."))
        );
        assert_eq!(
            open_path_from(
                ["--config-dir", "/cfg", "--flag", "."]
                    .into_iter()
                    .map(OsString::from)
            ),
            Some(PathBuf::from("."))
        );
        assert_eq!(
            open_path_from(["-psn_0_12345", "/work"].into_iter().map(OsString::from)),
            Some(PathBuf::from("/work"))
        );
        assert_eq!(
            open_path_from(
                [".", "--open-path", "/explicit"]
                    .into_iter()
                    .map(OsString::from)
            ),
            Some(PathBuf::from("/explicit"))
        );
        assert_eq!(open_path_from([OsString::from("--flag")].into_iter()), None);
    }

    #[test]
    fn finder_urls_become_directories() {
        assert_eq!(
            super::path_from_open_target("file:///Users/n/proj"),
            Some(PathBuf::from("/Users/n/proj"))
        );
        assert_eq!(
            super::path_from_open_target("file://localhost/Users/n/My%20Proj"),
            Some(PathBuf::from("/Users/n/My Proj"))
        );
        assert_eq!(
            super::path_from_open_target("file:///C:/work/app"),
            Some(PathBuf::from("C:/work/app"))
        );
        assert_eq!(super::path_from_open_target("-psn_0_1"), None);
        assert_eq!(super::path_from_open_target(""), None);

        let dir = std::env::temp_dir().join(format!("ntty7-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(super::directory_for_open(&file), dir);
        assert_eq!(super::directory_for_open(&dir), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ephemeral_runtime_dirname_is_not_a_config_dir() {
        assert!(super::inherited_runtime_dirname_is_ephemeral(Some(
            "ntty7-1234"
        )));
        assert!(!super::inherited_runtime_dirname_is_ephemeral(Some(
            "ntty7"
        )));
        assert!(!super::inherited_runtime_dirname_is_ephemeral(Some(
            "ntty7-custom"
        )));
        assert!(!super::inherited_runtime_dirname_is_ephemeral(None));
    }

    #[test]
    fn outer_tty7_zdotdir_restore_skips_the_redirector() {
        let home = std::ffi::OsStr::new("/Users/n");
        assert!(matches!(
            super::zdotdir_restore(Some(home)),
            super::ZdotdirRestore::Set(path) if path == home
        ));
        assert!(matches!(
            super::zdotdir_restore(Some(std::ffi::OsStr::new(""))),
            super::ZdotdirRestore::Clear
        ));
        assert!(matches!(
            super::zdotdir_restore(Some(std::ffi::OsStr::new("/tmp/tty7-zdotdir-4-1"))),
            super::ZdotdirRestore::Clear
        ));
        assert!(matches!(
            super::zdotdir_restore(None),
            super::ZdotdirRestore::Leave
        ));
        assert!(super::zdotdir_is_throwaway(std::ffi::OsStr::new(
            "/var/folders/xx/tty7-zdotdir-9-0"
        )));
        assert!(!super::zdotdir_is_throwaway(std::ffi::OsStr::new(
            "/Users/n"
        )));
    }

    #[test]
    fn a_cold_home_window_absorbs_only_the_first_open_urls() {
        let grace = super::LAUNCH_URL_GRACE;
        let young = std::time::Duration::from_millis(10);
        assert!(super::absorb_cold_open(true, false, false, young));
        assert!(super::absorb_cold_open(
            true,
            false,
            false,
            std::time::Duration::from_millis(499)
        ));
        assert!(!super::absorb_cold_open(true, false, false, grace));
        assert!(!super::absorb_cold_open(true, true, false, young));
        assert!(!super::absorb_cold_open(true, false, true, young));
        assert!(!super::absorb_cold_open(false, false, false, young));
    }

    #[test]
    fn empty_open_path_is_absent_and_relative_paths_are_directories() {
        assert_eq!(
            open_path_from(["--open-path", ""].into_iter().map(OsString::from)),
            None
        );
        assert_eq!(
            open_path_from([OsString::from("--open-path=")].into_iter()),
            None
        );
        assert_eq!(open_path_from([OsString::from("")].into_iter()), None);

        let cwd = std::env::current_dir().unwrap();
        assert_eq!(super::directory_for_open(std::path::Path::new(".")), cwd);
        assert_eq!(
            super::directory_for_open(std::path::Path::new("bar.rs")),
            cwd
        );
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            assert_eq!(
                super::directory_for_open(std::path::Path::new("~/ntty7-missing-file.rs")),
                PathBuf::from(home)
            );
        }
    }

    #[test]
    fn nested_ntty7_zdotdir_is_restored_and_a_custom_one_is_kept() {
        let home = std::ffi::OsStr::new("/Users/n");
        let custom = std::ffi::OsStr::new("/custom");
        let ntty7_tmp = std::ffi::OsStr::new("/tmp/ntty7-zdotdir-9-1");
        let tty7_tmp = std::ffi::OsStr::new("/tmp/tty7-zdotdir-4-1");
        assert!(matches!(
            super::plan_inherited_zdotdir(Some(ntty7_tmp), None, Some(home)),
            super::ZdotdirRestore::Set(path) if path == home
        ));
        assert!(matches!(
            super::plan_inherited_zdotdir(Some(custom), None, Some(home)),
            super::ZdotdirRestore::Leave
        ));
        assert!(matches!(
            super::plan_inherited_zdotdir(
                Some(ntty7_tmp),
                Some(home),
                Some(std::ffi::OsStr::new(""))
            ),
            super::ZdotdirRestore::Clear
        ));
        assert!(super::zdotdir_is_throwaway(ntty7_tmp));
        assert!(matches!(
            super::plan_inherited_zdotdir(Some(tty7_tmp), Some(home), Some(ntty7_tmp)),
            super::ZdotdirRestore::Set(path) if path == home
        ));
        assert!(matches!(
            super::plan_inherited_zdotdir(None, None, Some(home)),
            super::ZdotdirRestore::Set(path) if path == home
        ));
    }

    #[test]
    fn strip_drops_nested_ntty7_markers() {
        let mut cmd = std::process::Command::new("true");
        super::strip_inherited_terminal_env(&mut cmd);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|&(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in [
            "NTTY7",
            "NTTY7_USER_ZDOTDIR",
            "NTTY7_RC",
            "NTTY7_ZDOTDIR",
            "NTTY7_PANE",
        ] {
            assert!(removed.iter().any(|have| have == key), "missing {key}");
        }
    }
}
