//! Workspace identity the window still names.
//!
//! A pane is local. These types only decide whether a workspace the tests
//! (and a restored layout) still describe would have been routed somewhere
//! else, and what host id that name hashes to.

use std::fmt;

use serde::{Deserialize, Serialize};
use tty7_core::host::HostId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(uuid::Uuid);

impl WorkspaceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for WorkspaceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(WorkspaceId)
    }
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTarget {
    Profile {
        id: uuid::Uuid,
    },
    Alias {
        alias: String,
    },
    Direct {
        #[serde(default)]
        user: String,
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
    },
    Wsl {
        distro: String,
    },
    LocalStdio {
        program: String,
        args: Vec<String>,
    },
}

impl RemoteTarget {
    #[cfg(test)]
    pub fn direct(user: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self::Direct {
            user: user.into(),
            host: host.into().to_ascii_lowercase(),
            port,
        }
    }

    pub fn connection_key(&self) -> String {
        match self {
            RemoteTarget::Profile { id } => format!("ssh-profile:{id}"),
            RemoteTarget::Alias { alias } => format!("ssh-alias:{alias}"),
            RemoteTarget::Direct { user, host, port } => {
                format!("ssh-direct:{user}@{}:{port}", host.to_ascii_lowercase())
            }
            RemoteTarget::Wsl { distro } => format!("wsl:{distro}"),
            RemoteTarget::LocalStdio { program, args } => {
                format!("local-stdio:{program} {}", args.join(" "))
            }
        }
    }

    pub fn is_ssh(&self) -> bool {
        matches!(
            self,
            RemoteTarget::Profile { .. } | RemoteTarget::Alias { .. } | RemoteTarget::Direct { .. }
        )
    }

    pub fn host_id(&self) -> HostId {
        HostId::from_connection_key(&self.connection_key())
    }
}

impl fmt::Display for RemoteTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteTarget::Profile { id } => write!(f, "{id}"),
            RemoteTarget::Alias { alias } => write!(f, "{alias}"),
            RemoteTarget::Direct { user, host, port } => {
                if !user.is_empty() {
                    write!(f, "{user}@")?;
                }
                write!(f, "{host}")?;
                if *port != 22 {
                    write!(f, ":{port}")?;
                }
                Ok(())
            }
            RemoteTarget::Wsl { distro } => write!(f, "wsl:{distro}"),
            RemoteTarget::LocalStdio { program, .. } => {
                let name = std::path::Path::new(program)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| program.clone());
                write!(f, "local:{name}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg(test)]
pub struct RemoteRef {
    pub target: RemoteTarget,
    pub workspace: WorkspaceId,
}

#[cfg(test)]
impl RemoteRef {
    pub fn new(target: RemoteTarget, workspace: WorkspaceId) -> Self {
        Self { target, workspace }
    }
}
