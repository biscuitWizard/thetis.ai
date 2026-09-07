---
name = "Measuring and profiling mooR"
brief = "Prove a mooR optimisation instead of asserting it: where the benches are, how to profile with perf, and what zero-copy means in the db, var and kernel crates."
when_to_use = "Use when a change is meant to make mooR faster, when you must show a before and after, when something is slow and you do not know where, or when you must read runtime counters from a live server. Use it before you write a bench, run perf, or turn on trace_events. Not for choosing a correctness test or compiling and starting a server, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "performance", "benchmark", "profiling", "perf", "flamegraph", "tracing", "trace_events", "counters", "metrics", "optimisation", "slow", "micromeasure", "cache", "runtime counters", "chrome traces"]
related = ["moor/storage-and-state/storage-engine", "moor/execution/virtual-machine"]
version = 2
---
