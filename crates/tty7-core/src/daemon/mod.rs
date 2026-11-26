pub mod history;
pub mod pane;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod procinfo;
pub mod protocol;

pub(crate) const DETECTED_SHELL_ENV: &str = "NTTY7_DETECTED_SHELL";

pub(crate) mod shell_integration;

#[cfg(windows)]
pub(crate) mod winproc;

/// The Windows environment refresh new panes get (#333). Only the registry
/// reader and the spawn wiring are Windows-only; the merge itself is a pure
/// function, so `cfg(test)` keeps the module compiling everywhere and its
/// semantics tested on every platform's CI runner rather than only on Windows.
#[cfg(any(windows, test))]
pub(crate) mod windows_env;
