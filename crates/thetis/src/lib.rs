//! Thetis: the trusted kernel of the grip.
//!
//! It owns every capability the system has — the network, the filesystem, the
//! database, the build toolchain — and hands guests narrow, mediated slices of
//! them through the WIT contract in `wit/thetis.wit`. Guests (the agent, the
//! gateways, the tools) are hot-swappable WebAssembly components that can be
//! rebuilt, validated, and rolled back while the system keeps running.

pub mod activity;
pub mod admin;
pub mod aspect;
pub mod auth;
pub mod bindings;
pub mod branch_api;
pub mod branches;
pub mod branchops;
pub mod browser;
pub mod buildcache;
pub mod builder;
pub mod cache;
pub mod config;
pub mod control;
pub mod debug_api;
pub mod delegation;
pub mod devkit;
pub mod discord;
pub mod embeddings;
pub mod gateway;
pub mod gitctl;
pub mod grip;
pub mod host_api;
pub mod hostfs;
pub mod ipc;
pub mod llm;
pub mod loader;
pub mod manifest;
pub mod merge;
pub mod offload;
pub mod persist;
pub mod pipeline;
pub mod policy;
pub mod publish;
pub mod revisions;
pub mod roles;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod skill_index;
pub mod skill_lint;
pub mod skill_manager;
pub mod skills;
pub mod spill;
pub mod sshhosts;
pub mod store;
pub mod subagents;
pub mod system_api;
pub mod terminal;
pub mod transcripts;
pub mod watchdog;
pub mod watcher;
pub mod web;
pub mod workers;
pub mod workspace_api;
