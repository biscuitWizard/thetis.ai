---
name = "mooR builtin functions"
brief = "How bf_* builtins are numbered, registered, and called; the argument and error conventions; and the steps to add one with its docs and tests."
when_to_use = "Use when adding, changing, or debugging a builtin function in the mooR server: a bf_* module, the builtin id table, argument and error conventions, a builtin trampoline that calls MOO verbs, or a builtin-signature decode error. Not for the opcode loop or activation stack, the scheduler, the permission model in general, or the compiler. Not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "builtin", "bf_", "crates/kernel/src/vm/builtins", "crates/common/src/builtins.rs", "bfcallstate", "bfret", "bferr", "e_args", "e_type", "e_perm", "function_help", "function_info", "call_function", "bf_<name> override verb", "trampoline", "adding a builtin", "builtin signature mismatch"]
version = 2
---
