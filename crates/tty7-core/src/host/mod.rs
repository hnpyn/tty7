pub mod local;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread::ThreadId;

static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();

pub fn register_ui_thread() {
    let _ = UI_THREAD.set(std::thread::current().id());
}

pub fn is_ui_thread() -> bool {
    UI_THREAD
        .get()
        .is_some_and(|t| *t == std::thread::current().id())
}

#[inline]
pub fn guard_off_ui() {
    debug_assert!(
        !is_ui_thread(),
        "Host call on the UI thread — route it through ui::host_ops::HostOps"
    );
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub ignored: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub ignored: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ui_guard_is_inert_without_registration() {
        guard_off_ui();
    }
}
