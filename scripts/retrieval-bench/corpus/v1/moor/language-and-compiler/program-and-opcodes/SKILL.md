---
name = "Compiled programs and the MOO opcode set"
brief = "What a compiled MOO Program holds, how names and jump labels work, how StoredProgram is persisted and versioned, and what invalidates a cached program."
when_to_use = "Use when adding or changing an opcode, touching the stored program format, or a verb will not load after an upgrade with a decode or builtin-signature error. Not for what an opcode does when executed (read moor/execution/virtual-machine), not for MOO syntax (read compiler-pipeline), and not for value types (read value-model)."
universal = false
tags = ["moor", "moo", "opcode", "bytecode", "program", "prginner", "op", "name", "names", "label", "offset", "storedprogram", "moor_program", "flatbuffers", "jump label", "fork vector", "lambda", "program cache", "decode error", "builtin signature", "schema version", "code generation", "decompiler"]
version = 2
---
