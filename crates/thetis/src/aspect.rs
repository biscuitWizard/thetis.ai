//! Aspect identity.
//!
//! An *aspect* is one hot-swappable position in the running system: the agent,
//! a gateway, or a tool. Thetis took many forms and stayed herself throughout;
//! an aspect is one of the forms this system takes, which is why they are the
//! unit of building, versioning, health tracking and rollback.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Aspect {
    Agent,
    Gateway(String),
    Tool(String),
}

impl Aspect {
    pub fn tool(name: impl Into<String>) -> Self {
        Aspect::Tool(name.into())
    }

    pub fn gateway(name: impl Into<String>) -> Self {
        Aspect::Gateway(name.into())
    }

    /// Stable key used in redb and in the `/admin` UI.
    pub fn key(&self) -> String {
        match self {
            Aspect::Agent => "agent".to_string(),
            Aspect::Gateway(n) => format!("gateway/{n}"),
            Aspect::Tool(n) => format!("tool/{n}"),
        }
    }

    pub fn parse(key: &str) -> Result<Self> {
        match key.split_once('/') {
            None if key == "agent" => Ok(Aspect::Agent),
            Some(("gateway", n)) if !n.is_empty() => Ok(Aspect::Gateway(n.to_string())),
            Some(("tool", n)) if !n.is_empty() => Ok(Aspect::Tool(n.to_string())),
            _ => Err(anyhow!("unknown aspect key: {key}")),
        }
    }

    pub fn artifact_subdir(&self) -> String {
        match self {
            Aspect::Agent => "agent".to_string(),
            Aspect::Gateway(n) => format!("gateways/{n}"),
            Aspect::Tool(n) => format!("tools/{n}"),
        }
    }

    /// Cargo package name of the source crate backing this aspect.
    pub fn crate_name(&self) -> String {
        match self {
            Aspect::Agent => "agent-core".to_string(),
            Aspect::Gateway(n) => format!("gateway-{n}"),
            Aspect::Tool(n) => format!("tool-{n}"),
        }
    }

    /// Filename cargo emits for the built component.
    pub fn wasm_filename(&self) -> String {
        format!("{}.wasm", self.crate_name().replace('-', "_"))
    }
}

impl fmt::Display for Aspect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// Names must be safe to use as a directory, a cargo package name, and a tool
/// name exposed to the model.
pub fn validate_component_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 48 {
        return Err(anyhow!("name must be 1-48 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(anyhow!(
            "name must contain only lowercase letters, digits, and hyphens"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(anyhow!("name must not start or end with a hyphen"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_keys_round_trip() {
        for aspect in [
            Aspect::Agent,
            Aspect::gateway("web"),
            Aspect::tool("weather-lookup"),
        ] {
            assert_eq!(Aspect::parse(&aspect.key()).unwrap(), aspect);
        }
    }

    #[test]
    fn rejects_unsafe_names() {
        for bad in ["", "Has-Upper", "has_underscore", "-lead", "trail-", "../x"] {
            assert!(validate_component_name(bad).is_err(), "accepted {bad:?}");
        }
        assert!(validate_component_name("weather-2").is_ok());
    }
}
