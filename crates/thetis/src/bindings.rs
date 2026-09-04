//! Generated bindings for the three guest worlds.
//!
//! The `agent` world imports every host interface, so its generated modules are
//! the canonical definitions; the `gateway` and `tool` worlds reuse them via
//! `with:` so there is exactly one `Host` trait per interface to implement and
//! one Rust type per WIT type.

#![allow(clippy::all)]

pub mod agent {
    wasmtime::component::bindgen!({
        world: "agent",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
}

/// Interfaces the host implements ahead of the guest world that will import
/// them. See `world host-staging` in the contract for why this exists: it lets
/// the orchestrator answer a new import *before* any guest is compiled against
/// it, which is the ordering that keeps a contract change recoverable.
///
/// Empty between contract additions, which is the normal state. To stage the
/// next one: add `import <iface>;` to `world host-staging`, re-add a `with:`
/// mapping here for any shared type the interface names (bindgen rejects a
/// `with` entry the world does not reference, so it cannot be left behind),
/// re-export the module below, implement the trait, build, restart — and only
/// then move the import into `world agent`.
pub mod staging {
    wasmtime::component::bindgen!({
        world: "host-staging",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
}

pub mod gateway {
    wasmtime::component::bindgen!({
        world: "gateway",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
        with: {
            "thetis:grip/types": crate::bindings::agent::thetis::grip::types,
            "thetis:grip/sys": crate::bindings::agent::thetis::grip::sys,
            "thetis:grip/session": crate::bindings::agent::thetis::grip::session,
        },
    });
}

pub mod tool {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "../../wit",
        imports: { default: async | trappable },
        exports: { default: async },
        additional_derives: [serde::Serialize, serde::Deserialize],
        with: {
            "thetis:grip/types": crate::bindings::agent::thetis::grip::types,
            "thetis:grip/sys": crate::bindings::agent::thetis::grip::sys,
            "thetis:grip/sandbox": crate::bindings::agent::thetis::grip::sandbox,
        },
    });
}

/// Canonical shared types (records, variants) used throughout the host.
#[allow(unused_imports)]
pub use agent::thetis::grip::types;

/// Host interface modules, each exposing a `Host` trait the orchestrator
/// implements and an `add_to_linker` used when building a linker.
#[allow(unused_imports)]
pub use agent::thetis::grip::{
    branch, configuration, control, delegation, devkit, hostfs, llm, sandbox, session, skills, sys,
    terminal, tooling, transcripts,
};

/// The gateway's read-only view of the skill corpus. It lives in the gateway
/// world rather than the agent's, because only the gateway imports it.
#[allow(unused_imports)]
pub use gateway::thetis::grip::skills_view;

/// The operator's controls, for the chat surface's control panel. Gateway-only
/// for the same reason as `skills-view`: the agent has `configuration` and
/// `control` of its own, scoped by policy rather than by administrator.
#[allow(unused_imports)]
pub use gateway::thetis::grip::admin;
