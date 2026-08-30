---
name = "The mooR task scheduler"
brief = "How a MOO task starts, runs, suspends, forks, retries after a conflict, and dies; the queues behind it and the tick and seconds limits that bound it."
when_to_use = "Use when the question is about task lifecycle in the mooR server: what starts a task, suspend and resume, background tasks, conflict retry and backoff, surviving a restart, or the tick/seconds limits on $server_options. Not for opcode execution, writing a builtin, permission checks, or command parsing, and not for Torchship or Thetis's internals."
universal = false
tags = ["moor", "scheduler", "task", "suspend", "fork", "resume", "kill_task", "ticks", "server_options", "conflict retry", "backoff", "background task", "checkpoint", "dump_interval", "queued_tasks", "task_recv", "task_send", "wait_task", "task ids", "fg_ticks", "bg_ticks", "fg_seconds", "bg_seconds", "max_stack_depth", "max_task_retries", "read()"]
version = 2
---
