---
name = "Testing mooR"
brief = "Choose and run the right mooR test: cargo unit tests, .moot text tests, cross-process integration tests, benches, Elle consistency runs, and what CI gates."
when_to_use = "Use when you must add a test, decide whether a behaviour belongs in a Rust test or a .moot file, run a single test, or explain a test that passes alone and fails in the workspace. Use it before you claim a change works. Not for compiling or starting a server, or for style and pull-request rules, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "test", "moot", "cargo test", "integration test", "proptest", "bench", "elle", "jepsen", "regression", "ci", "flaky", "test something that spans the daemon and a host", "licence headers"]
related = ["moor/execution/task-scheduler", "moor/storage-and-state/transactions"]
version = 2
---
