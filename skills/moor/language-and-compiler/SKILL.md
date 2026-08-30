---
name = "The MOO language and its compiler"
brief = "How mooR defines and implements MOO: the compile and decompile path, the Var value model, the compiled Program format, and the LambdaMOO compatibility switches."
when_to_use = "Use when changing or explaining the engine side of the MOO language: the compiler, the value model, the opcode set, or a compile error a user reported. Not for writing MOO verbs (read torchship/torchship-programming/moor-book), not for how the VM executes an opcode (read moor/execution/virtual-machine), and not for the Torchship database or Thetis internals."
universal = false
tags = ["moor", "moo", "compiler", "language", "parser", "bytecode", "opcodes", "var", "value types", "symbol", "flyweight", "decompile", "unparse", "lambdamoo compatibility", "compile error", "moor-compiler", "moor-var", "lexer", "cst", "ast", "codegen", "diagnostics", "flatbuffer", "compileoptions", "featuresconfig"]
children = "auto"
related = ["moor/execution/virtual-machine", "moor/content-pipeline/objdef-format"]
version = 2
---

# The MOO language and its compiler

This skill is written in ASD-STE100 Simplified Technical English.

Two crates own the language. `crates/compiler` (`moor-compiler`) turns MOO source
into a `Program` and turns a `Program` back into source. `crates/var` (`moor-var`)
defines every value the language can hold, and the shape of a compiled program.
Everything else in the server consumes these two.

This skill is a dispatch table. The substance is in the children.

## The one fact that shapes the whole area

**The database stores compiled programs, not source text.** When an author asks a
verb for its code, the server decompiles the stored `Program` back to an abstract
syntax tree and prints it. The source the author typed is gone at the moment the
verb is programmed. `verb_code()` and the objdef exporter both work this way.

Every rule in this topic follows from that. The opcode set must be decompilable. The
unparser must produce source that compiles to the same program. The stored format
must be readable by a later build of the server, because the database outlives the
binary that wrote it.

## Which child to read

- [compiler-pipeline](skill:moor/language-and-compiler/compiler-pipeline) —
  how source becomes bytecode and bytecode becomes source again, why there is
  a concrete syntax tree and an abstract one, and a bad compile error or wrong
  line number.
- [value-model](skill:moor/language-and-compiler/value-model) — what a `Var`
  is and what an operation on one costs, symbols and flyweights, why `"A" ==
  "a"`, and how errors work as values.
- [program-and-opcodes](skill:moor/language-and-compiler/program-and-opcodes)
  — what a compiled `Program` holds, adding or changing an opcode, and a
  stored program that will not decode after an upgrade.
- [language-features-and-compat](skill:moor/language-and-compiler/language-features-and-compat)
  — which features are mooR extensions rather than LambdaMOO, how a feature
  is switched on or off, and a core that will not import.

## The boundary with verb authoring

`torchship/torchship-programming/moor-book` teaches MOO as a language for writing
verbs: what the syntax means, which builtins to call, what surprises a verb author.
This topic teaches how the engine defines and implements that language: what the
compiler does with the syntax, what a value is in memory, what the bytecode looks
like, and which parts are mooR extensions rather than LambdaMOO. If the question is
"how do I write this verb", it is not here.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/compiler/src/lexer.rs` | Tokens and spans. Total: it never fails. |
| `crates/compiler/src/frontend/` | The parser, the lossless CST, and lowering to the AST. |
| `crates/compiler/src/ast.rs` | The abstract syntax tree the backend and the decompiler share. |
| `crates/compiler/src/backend/` | Code generation. |
| `crates/compiler/src/decompile/` | `Program` back to an AST. |
| `crates/compiler/src/unparse/` | AST back to source lines, and value literals. |
| `crates/compiler/src/objdef.rs`, `objdef_literal.rs` | Object definition files and the value literal parser. |
| `crates/var/src/` | `Var` and every value type. |
| `crates/var/src/program/` | `Program`, `Op`, `Names`, `Label`, `StoredProgram`. |
| `crates/schema/src/convert_program.rs`, `opcode_stream.rs` | The persisted program format. |

The persisted format lives in `moor-schema`, not in `moor-var`. A comment in
`crates/var/src/program/stored_program.rs` says decoding happens in `moor-compiler`.
That is out of date; it happens in `moor-schema`.

## The rules a language change must satisfy

State these to yourself before you start. Each child gives the detail.

1. **The round trip holds.** Source that compiles must decompile and unparse to
   source that compiles to an equivalent program. `compiler-pipeline`.
2. **The opcode set stays decompilable.** An opcode whose intent cannot be recovered
   from the instruction stream breaks `verb_code()`. `program-and-opcodes`.
3. **The persisted format stays readable.** Opcode numbers are fixed forever, and
   the stored version has a supported range. `program-and-opcodes`.
4. **The builtin table is part of the stored format.** A program records a signature
   of the builtins it calls. Changing a builtin's shape invalidates stored programs
   that call it. `program-and-opcodes`, and `moor/execution/builtin-functions`.
5. **A new surface syntax needs a switch, or it is not compatible.** Anything a
   LambdaMOO core would not expect is gated. `language-features-and-compat`.
6. **A value change reaches the database.** Values are stored, so a new value type
   needs an encoding in `moor-schema` as well as a literal form.
   `moor/services/wire-schema`.

## Knowledge barriers

Do not edit this area before you understand these. None is learned from the code.

| Barrier | Where it is taught |
|---|---|
| That stored bytecode is the only copy of a verb's code | This skill, above; `compiler-pipeline` |
| Immutable values with structural sharing, and why copy-on-write is cheap | `value-model` |
| Case-insensitive equality on strings and symbols, and where it does not apply | `value-model` |
| Rowan-style green trees: a lossless CST with a typed view on top | `compiler-pipeline` |
| Schema evolution against data already on disk | `program-and-opcodes`, `moor/services/wire-schema` |
| What LambdaMOO compatibility does and does not promise | `language-features-and-compat`, `moor/content-pipeline/textdump-compat` |
| How a `Program` is executed once it exists | `moor/execution/virtual-machine` |

## Read first / read next

Read `moor` and its `references/glossary.md` first, because "verb", "program" and
"value" each name more than one thing in this codebase.

After this topic, read `moor/execution/virtual-machine` for what consumes a
`Program`, `moor/content-pipeline/objdef-format` for how source reaches the database
from files, and `moor/working-in-the-repo/testing` for the Moot harness that asserts
language behaviour.
