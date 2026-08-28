//! Process roles.
//!
//! One binary, two jobs: `thetis` runs the gateway (web, database, worker
//! supervision); `thetis worker` runs a conversation orchestrator against a
//! source checkout, wired to its parent gateway through an inherited socket.

pub mod gateway;
pub mod worker;
