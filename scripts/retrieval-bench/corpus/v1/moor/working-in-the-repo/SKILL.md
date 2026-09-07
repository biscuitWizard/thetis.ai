---
name = "Working in the mooR repository"
brief = "Build, run, test and contribute to the mooR Rust workspace: which crate makes which binary, the checks CI enforces, and the rules a patch must satisfy."
when_to_use = "Use when the task is about the repository itself: compiling, starting a server locally, choosing a test to write, or a build that fails only in CI. Use it before you claim a change works. Not for how the database, the compiler, the VM or the RPC protocol behave, which are the other moor topics, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "build", "cargo", "workspace", "test", "clippy", "rustfmt", "dprint", "ci", "contributing", "pull request", "rust", "licence headers", "dependency policy", "commit norms"]
children = "auto"
related = ["moor/services/daemon-and-rpc"]
version = 2
---
