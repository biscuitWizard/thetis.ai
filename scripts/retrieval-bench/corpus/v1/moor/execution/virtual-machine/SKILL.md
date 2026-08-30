---
name = "The mooR bytecode VM"
brief = "How the MOO interpreter runs a verb: activations and frames, verb call setup, tick slices, error raising and stack unwinding."
when_to_use = "Use when working on MOO code execution inside the mooR server: the activation stack, how a verb or lambda call is set up, opcode dispatch and tick counting, try/catch/finally unwinding, or a stack-depth or execution-result question. Not for the compiler or opcode set itself, writing a builtin, scheduler queues, or permission rules, and not for the Torchship database or Thetis's own internals."
universal = false
tags = ["moor", "vm", "interpreter", "activation", "frame", "execstate", "moostackframe", "value stack", "scope stack", "unwind", "traceback", "e_maxrec", "verb d flag", "opcode", "tick", "try catch finally", "program cache", "executionresult"]
version = 2
---
