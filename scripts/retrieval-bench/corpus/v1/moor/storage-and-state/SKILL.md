---
name = "The mooR object store"
brief = "Pick the right part of mooR's transactional object database: transactions and conflict retry, the object model, the storage engine, and object lifetime."
when_to_use = "Use for work in mooR's database layer: commit conflicts, relations, property and verb resolution, caches, snapshots, checkpoints, or garbage collection. Use it to choose the child skill. Not for MOO language or compiler questions, the scheduler's own queues and limits, the objdef or textdump file formats, the Torchship game database, or Thetis's own internals."
universal = false
tags = ["moor", "moo", "database", "transactions", "worldstate", "loaderinterface", "moor-db", "mvcc", "conflict", "commitresult::conflictretry", "retry", "objects", "properties", "verbs", "anonymous objects", "fjall", "snapshot", "checkpoint", "garbage collection", "storage"]
children = "auto"
related = ["moor/execution/task-scheduler", "moor/execution/permissions-and-security"]
version = 2
---
