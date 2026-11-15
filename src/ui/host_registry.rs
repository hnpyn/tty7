use std::collections::HashMap;

use gpui::{App, Global};
use tty7_core::host::local::LocalHost;

pub use tty7_core::host::{HostId, SharedHost};

pub struct HostRegistry {
    hosts: HashMap<HostId, SharedHost>,
}

impl Global for HostRegistry {}

impl Default for HostRegistry {
    fn default() -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(HostId::LOCAL, LocalHost::shared());
        HostRegistry { hosts }
    }
}

impl HostRegistry {
    pub fn lookup(cx: &App, id: HostId) -> Option<SharedHost> {
        match cx.try_global::<HostRegistry>() {
            Some(reg) => reg.hosts.get(&id).cloned(),
            None => id.is_local().then(LocalHost::shared),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_registry_already_holds_the_local_host() {
        let reg = HostRegistry::default();
        let local = reg.hosts.get(&HostId::LOCAL).expect("local is present");
        assert!(local.id().is_local());
        assert_eq!(reg.hosts.len(), 1);
    }

    #[test]
    fn remote_hosts_come_and_go_but_local_stays() {
        let mut reg = HostRegistry::default();
        let id = HostId::from_connection_key("ssh-direct:me@box:22");
        reg.hosts.insert(id, LocalHost::new());
        assert_eq!(reg.hosts.len(), 2);

        assert!(reg.hosts.remove(&id).is_some());
        assert!(reg.hosts.contains_key(&HostId::LOCAL));
        assert!(!reg.hosts.contains_key(&id));
    }
}
