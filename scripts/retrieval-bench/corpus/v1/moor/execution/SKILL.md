---
name = "Running verbs in mooR"
brief = "The parts of the mooR server that execute MOO code: tasks, the scheduler, the bytecode VM, builtin functions, permissions, and the command parser."
when_to_use = "Use when a change touches the running of MOO code in the mooR server: task suspend, fork, resume, transaction conflict retry, the activation stack, a builtin function, wizard and programmer bits, or how a command becomes a verb call. Use it to pick which child to read. Not for the database engine, the compiler and opcode set, or the RPC layer, and not for Torchship or Thetis's own internals."
universal = false
tags = ["moor", "execution", "task", "scheduler", "vm", "builtin", "permissions", "command parser", "suspend", "fork", "kill", "ticks", "seconds limits", "e_maxrec", "activation stack", "wizard", "programmer bits", "set_task_perms", "capability grants", "verb call"]
children = "auto"
version = 3
---
