//! Discovery and validation of secondary gateway HTTP mounts.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::{aspect::Aspect, config::Config};

pub const MOUNT_FILE: &str = "gateway.toml";
pub const RESERVED_PREFIXES: &[&str] = &[
    "/ws",
    "/login",
    "/logout",
    "/api",
    "/admin",
    "/preview",
    "/workspace",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMount {
    pub gateway: String,
    pub path: String,
}

impl GatewayMount {
    pub fn aspect(&self) -> Aspect {
        Aspect::gateway(&self.gateway)
    }
}

#[derive(Deserialize)]
struct MountFile {
    mount: String,
}

pub fn parse_mount(gateway: &str, text: &str) -> Result<GatewayMount> {
    let file: MountFile = toml::from_str(text)?;
    validate_path(&file.mount)?;
    Ok(GatewayMount {
        gateway: gateway.to_string(),
        path: file.mount,
    })
}

pub fn validate_path(path: &str) -> Result<()> {
    if path.len() > 64 || !path.starts_with('/') || path == "/" || path.ends_with('/') {
        bail!("mount must start with '/', have no trailing slash, and be at most 64 characters");
    }
    if RESERVED_PREFIXES.iter().any(|reserved| {
        path == *reserved
            || path
                .strip_prefix(reserved)
                .is_some_and(|rest| rest.starts_with('/'))
    }) {
        bail!("mount uses a reserved prefix");
    }
    if path[1..].split('/').any(|segment| {
        segment.is_empty()
            || !segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }) {
        bail!("mount segments may contain only lowercase ASCII letters, digits, and '-'");
    }
    Ok(())
}

pub fn discover(cfg: &Config) -> Vec<GatewayMount> {
    let mut found = Vec::new();
    for aspect in crate::pipeline::discover_aspects(cfg) {
        let Aspect::Gateway(name) = &aspect else {
            continue;
        };
        let file = cfg.aspect_source_dir(&aspect).join(MOUNT_FILE);
        if !file.is_file() {
            continue;
        }
        if name == &cfg.primary_gateway {
            tracing::warn!(path = %file.display(), "primary gateway mount is ignored");
            continue;
        }
        match std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse_mount(name, &text))
        {
            Ok(mount) => found.push(mount),
            Err(error) => {
                tracing::warn!(%error, path = %file.display(), gateway = %name, "invalid gateway mount; skipping")
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    let mut paths = HashSet::new();
    found.retain(|mount| {
        if paths.insert(mount.path.clone()) { true } else {
            tracing::warn!(path = %mount.path, gateway = %mount.gateway, "duplicate gateway mount; skipping");
            false
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_file_parses() {
        assert_eq!(
            parse_mount("campaign", "mount = \"/play\"\n").unwrap(),
            GatewayMount {
                gateway: "campaign".into(),
                path: "/play".into()
            }
        );
    }

    #[test]
    fn invalid_and_reserved_paths_are_rejected() {
        for path in ["/", "play", "/play/", "/admin", "/api/x", "/Play"] {
            assert!(validate_path(path).is_err(), "accepted {path}");
        }
    }
}
