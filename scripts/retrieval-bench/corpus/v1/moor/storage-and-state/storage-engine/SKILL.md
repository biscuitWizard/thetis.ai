---
name = "The storage engine and durability"
brief = "What mooR keeps in memory versus on disk, how a commit reaches the fjall key-value store, what a snapshot and a checkpoint are, and what a crash costs."
when_to_use = "Use for questions about mooR's persistence: memory use, the on-disk format, durability guarantees, snapshots, or what survives a crash or restart. Not for conflict detection and retry, the relation model, or object lifetime, which the sibling skills own, and not for the event log or the Torchship database."
universal = false
tags = ["moor", "database", "storage", "fjall", "durability", "fsync", "wait_for_persistence", "persistence", "batch writer", "backpressure", "snapshot", "checkpoint", "exports", "migration", "startup time", "databaseconfig", "restart", "crash", "memory", "database size", "keyspace", "moor-db"]
version = 2
---
