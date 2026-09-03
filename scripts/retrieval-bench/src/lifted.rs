//! The lifted orchestrator source: real files, copied in by the runner.
//!
//! These are ordinary module files rather than `include!`s. That is not a style
//! preference — an `include!` inside a `mod { }` block rejects the inner doc
//! comments (`//!`) that head every one of these files, and stripping those to
//! make the include work would mean editing the copies, which is exactly what
//! this design refuses to do. As module files they compile verbatim, so what is
//! measured is provably the shipping source and not a paraphrase of it.
//!
//! Populated by run.sh from `crates/thetis/src/` and, for `table`, by extract.py
//! from `agents/agent-core/src/groups.rs`. Absent from git on purpose: they are
//! build artifacts whose content depends on which revision is being measured.

pub mod skills;

pub mod skill_lint;

pub mod skill_index;

/// The tool-group table. Optional because it postdates the ranker: an older
/// revision measures SkillRet alone rather than failing to build.
#[cfg(feature = "toolret")]
pub mod table;
