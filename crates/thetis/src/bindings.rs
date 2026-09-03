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
    branch, configuration, control, devkit, hostfs, llm, sandbox, session, skills, sys,
    terminal, tooling,
};

/// The gateway's read-only view of the skill corpus. It lives in the gateway
/// world rather than the agent's, because only the gateway imports it.
#[allow(unused_imports)]
pub use gateway::thetis::grip::skills_view;
