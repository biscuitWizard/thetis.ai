---
name = "The MOO compile and decompile pipeline"
brief = "Follow MOO source from text to bytecode and back: lexer, lossless CST, AST lowering, code generation, decompile and unparse, and the round-trip rule they obey."
when_to_use = "Use when working in the compiler crate, or a compile error is wrong, verb_code() returns something the author did not write, or a line number is off. Not for what an opcode does at run time (read moor/execution/virtual-machine), not for value types (read value-model), and not for persisted program bytes (read program-and-opcodes)."
universal = false
tags = ["moor", "moo", "compiler", "parser", "lexer", "cst", "ast", "codegen", "decompile", "unparse", "verb_code", "compile error", "line numbers", "rowan", "round trip", "crates/compiler", "diagnostics", "adding syntax"]
version = 2
---
