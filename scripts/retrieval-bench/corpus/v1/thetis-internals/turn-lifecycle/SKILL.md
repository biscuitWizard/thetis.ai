---
name = "The turn lifecycle"
brief = "What happens in one agent turn: rehydration from the event log, prompt assembly, streaming, tool dispatch, nudges, and resume after a restart."
when_to_use = "Use when you must understand or change how a turn runs: the order of the steps in handle-turn, how messages are rebuilt from the log, what stops a turn, how a nudge or a cancel arrives mid-turn, how usage and cost are counted, or why a turn continues after a restart. Not for compaction internals, which have their own child skill."
universal = false
tags = ["turn", "loop", "rehydrate", "event log", "nudge", "cancel", "streaming", "tool dispatch", "resume", "session", "tool-group:selfmod"]
version = 1
---
