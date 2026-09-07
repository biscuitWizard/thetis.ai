---
name = "Object lifecycle and garbage collection"
brief = "How mooR creates, recycles, renumbers and collects objects, what makes an anonymous object unreachable, and why object references are unsafe to hold."
when_to_use = "Use when work touches object creation, recycle, renumber, anonymous objects, or the mark-and-sweep collector, or an object that vanished or should have vanished. Not for transaction conflict, the relation model, or the on-disk format, which the sibling skills own, and not for the Torchship database."
universal = false
tags = ["moor", "objects", "create", "recycle", "renumber", "object number allocation", "anonymous objects", "garbage collection", "gc", "gc_interval", "mark and sweep", "gc mark thread", "sweep pause", "object numbers", "uuid objects", "reachability", "gcinterface", "moor-db"]
version = 2
---
