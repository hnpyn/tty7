//! Home directory of the machine this window is running on.

use std::path::PathBuf;

/// `USERPROFILE` is the fallback rather than the only source on Windows, so
/// MSYS and Git Bash environments that export `HOME` keep working.
pub(crate) fn local_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}
