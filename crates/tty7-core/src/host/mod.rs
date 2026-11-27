pub mod local;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread::ThreadId;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct HostId(pub u64);

impl HostId {
    pub const LOCAL: HostId = HostId(0);

    pub fn from_connection_key(key: &str) -> HostId {
        let h = fnv1a64(key.as_bytes());
        HostId(if h == 0 { 1 } else { h })
    }

    pub fn is_local(self) -> bool {
        self == HostId::LOCAL
    }
}

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

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
    fn fnv1a64_matches_the_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn zero_is_reserved_for_local() {
        assert!(HostId::LOCAL.is_local());
        for i in 0..2000u32 {
            let key = format!("ssh-direct:me@box{i}:22");
            assert!(!HostId::from_connection_key(&key).is_local());
        }
        assert!(!HostId::from_connection_key("").is_local());
    }

    #[test]
    fn connection_keys_map_to_stable_ids() {
        let a = HostId::from_connection_key("ssh-direct:me@box:22");
        assert_eq!(a, HostId::from_connection_key("ssh-direct:me@box:22"));
        assert_ne!(a, HostId::from_connection_key("ssh-direct:me@box:2222"));
        assert_ne!(a, HostId::from_connection_key("wsl:Ubuntu"));
    }

    #[test]
    fn the_ui_guard_is_inert_without_registration() {
        guard_off_ui();
    }
}
