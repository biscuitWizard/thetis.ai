---
name = "The MOO compile and decompile pipeline"
brief = "Follow MOO source from text to bytecode and back: lexer, lossless CST, AST lowering, code generation, decompile and unparse, and the round-trip rule they obey."
when_to_use = "Use when working in crates/compiler: the lexer, the handwritten parser and its rowan CST, lowering to the AST, the codegen backend, the decompiler, the unparser, or diagnostics. Use it when a compile error is wrong or unhelpful, when verb_code() returns something the author did not write, when a reported line number is off, or when adding syntax and you must decide which stage owns it. Not for what an opcode does at run time; that is moor/execution/virtual-machine. Not for the Var value types, which are value-model. Not for the persisted program bytes, which are program-and-opcodes. Not for writing MOO verbs in a game, and not for the Torchship database. Not for Thetis internals."
universal = false
tags = ["moor", "moo", "compiler", "parser", "lexer", "cst", "ast", "codegen", "decompile", "unparse", "verb_code", "compile error", "line numbers", "rowan", "round trip"]
version = 1
---

# The MOO compile and decompile pipeline

`moor-compiler` is a two-way pipeline. Forward it turns verb source into a
`Program`. Backward it turns a `Program` into source lines. Both directions are
used at run time, in a live world, on code an author is editing.

## Why both directions run in production

The database holds compiled programs. It does not hold verb source. When an author
edits a verb, the server compiles the new text and stores the result; the text is
discarded. When an author asks to see a verb, the server decompiles the stored
program and prints it.

The forward path runs from `WorldStateAction::ProgramVerb` in
`crates/kernel/src/tasks/world_state_executor.rs`, and from the objdef and textdump
importers. The backward path runs from `bf_verb_code` and `bf_verb_code_hash` in
`crates/kernel/src/vm/builtins/bf_verbs.rs`, from the same world-state executor, and
from the objdef exporter in `crates/objdef/src/write.rs`.

So a defect in the backward path is not a developer inconvenience. It is data loss
from the author's point of view.

## The forward stages

| Stage | Consumes | Produces | Module |
|---|---|---|---|
| Lex | Source text | A token vector with byte spans, including trivia and a final `Eof` | `lexer.rs` |
| Parse | Tokens | A rowan green tree plus a list of parse errors | `frontend/parser.rs`, `frontend/syntax.rs` |
| Type the tree | Green tree | Typed CST node wrappers | `frontend/cst.rs` |
| Lower | Typed CST | `Parse`: AST statements, a `VarScope`, and bound `Names` | `frontend/lower.rs` |
| Generate | `Parse` | `Program` | `codegen.rs`, `backend/` |

`compile()` in `codegen.rs` is the only entry point most callers need. It takes the
source and a `CompileOptions`, and it runs every stage.

### The lexer is total

The lexer never returns an error. Unrecognised input becomes an `Error` token, and
whitespace, newlines and comments become trivia tokens. The token vector covers the
whole input, byte for byte, and always ends with `Eof`. This is what makes the tree
above it lossless.

### Two trees, and why

The parser builds a **concrete syntax tree**: a rowan green tree of `SyntaxKind`
nodes and tokens that contains every byte of the source, trivia included. Lowering
then walks it and builds an **abstract syntax tree** in `ast.rs`, which keeps only
what code generation and decompilation need.

The CST exists for three reasons:

1. **Error recovery.** The parser records an error and keeps going. It produces a
   tree for input it could not fully understand, and a list of every error, with
   spans. A single-pass parser that returns on the first error cannot do this.
2. **Spans survive.** Every node knows its byte range, so a diagnostic can point at
   the exact text. The AST carries only line and column.
3. **Losslessness is available.** A tool that must show the author their own text,
   with their own comments and spacing, has a tree that still contains it.

Point 3 is a capability, not a current behaviour. Nothing in the repository yet
consumes the CST to reproduce original source. `parse_to_cst` and `parse_to_syntax_node`
are exported from the crate and used only inside it. The path that actually reaches
authors runs through the AST, and therefore drops comments and formatting. If you
are asked why comments vanish from a verb, that is the reason, and the CST is where
a fix would start.

### Lowering is where semantics enter

`frontend/lower.rs` is the largest single stage, and it does more than convert node
shapes:

- It resolves identifiers against a `VarScope`, which builds the scope tree and
  assigns each variable a `Variable` and then a `Name`. See `program-and-opcodes`.
- It applies `CompileOptions`. Every `CompileError::DisabledFeature` is raised here.
  See `language-features-and-compat`.
- It decides whether an undeclared identifier binds globally or in the current
  scope. Outside a lambda body it binds to the verb-global scope, which is
  LambdaMOO's rule. Inside a lambda body it binds to the enclosing lexical scope.
- It calls `annotate_line_numbers`, which is described below.

### Code generation

`backend/` splits code generation into small pieces that share one `CodegenState`:
`emitter` holds the opcode vector and jump labels, `operands` holds the side tables
that opcodes index, `stack` tracks value-stack and scope depth, and `control` tracks
loop and try frames. `expr_codegen` and `stmt_codegen` walk the AST; `lambda_codegen`
compiles a lambda body into its own nested `Program`.

Two behaviours matter beyond the obvious:

- **The emitter does one peephole fusion.** A `Put` followed by `Pop` becomes
  `PutPop`, and likewise for the scope-0 and temporary variants, but only when no
  jump label points at the position between them. Any optimisation you add must
  respect the same rule, and must still decompile.
- **Literal pooling is case sensitive.** `operands.add_literal` compares with
  `eq_case_sensitive`, not with `==`. MOO string equality ignores case, so pooling on
  `==` would silently replace `"Foo"` with `"foo"` in the program's literal table.
  See `value-model`.

Code generation ends with two assertions: the value stack must be empty and the scope
stack must be empty. Both panic if not. A panic here means a codegen bug, not bad
user input.

## The backward stages

| Stage | Consumes | Produces | Module |
|---|---|---|---|
| Decompile | `Program` | `Parse` (an AST, plus the program's `Names`) | `decompile/` |
| Annotate | AST | The same AST with line numbers | `unparse::annotate_line_numbers` |
| Unparse | `Parse` | A vector of source lines | `unparse/` |

`program_to_tree` walks the opcode vector and rebuilds statements and expressions
from an expression stack and the jump structure. It reads the program's `Names` to
recover identifiers, and it reconstructs lambda bodies by recursing into the nested
programs the `MakeLambda` opcode points at.

`unparse` takes a `Parse` and writes source. It has two switches: fully parenthesise,
and indent. `bf_verb_code` exposes both to MOO. Parenthesisation uses
`precedence.rs`, which is deliberately shared with the parser. One table drives both
directions, so the unparser adds a parenthesis exactly where the parser would need
one.

## The round trip, stated precisely

The project holds itself to this property:

> Source that compiles must decompile and unparse to source that compiles to an
> equivalent program, and unparsing that program again must produce the same text.

It is checked in three places:

- `crates/compiler/src/decompile/mod.rs` tests compile a program, decompile it, and
  compare the resulting tree against the tree from a direct parse, using
  `assert_trees_match_recursive`.
- `crates/compiler/src/unparse/mod.rs` tests parse, unparse, and compare text.
- `crates/compiler/src/tests/proptest/` generates random ASTs, formats them, and
  checks that parse, unparse, parse, unparse is stable. A failing case is written to
  `crates/compiler/proptest-failures/` for later inspection.

### What the property forbids

- An opcode sequence whose source intent cannot be recovered. If you cannot write the
  decompiler arm, the opcode is not admissible.
- Surface syntax that lowers to something the unparser cannot print back. The
  unparser must gain a case in the same change.
- Any optimisation that erases structure the decompiler depends on, unless the
  decompiler learns to reconstruct it.
- Two different surface forms that compile to the same opcodes, unless you accept
  that one of them is unwritable and the author will see the other. This already
  happens: `fn name(...) endfn` and an assigned lambda share a representation, and
  the decompiler chooses which to print from the lambda's self-reference variable.

## Line numbers are the unparsed line numbers

`annotate_line_numbers` assigns each statement the line it *would* occupy in
unparsed output, and code generation records those numbers into the program's line
number spans. So the line reported in a traceback is a line of the canonical printed
form of the verb, not a line of whatever the author typed.

This is coherent, because the author is shown that same canonical form by
`verb_code()`. It is confusing when source arrives from a file: a `.moo` objdef file
with blank lines or comments will report line numbers that do not match the file. If
someone says traceback line numbers are wrong, check this before you look for a bug.

## Diagnostics

`diagnostics.rs` renders a `CompileError` three ways: a one-line summary, a summary
with source context, and source context plus notes. Rendering uses `ariadne` and can
emit graphics and colour. `compile_error_to_map` turns an error into a MOO map so a
core can inspect it. `CompileError` itself is defined in
`crates/common/src/model/mod.rs`, not in the compiler crate, because the database
and the RPC layer both carry it.

Parse errors carry a `ParseErrorDetails` with a byte span, the expected token names,
and free-text notes. Fill these in when you add a parse error; they are what makes
the rendered diagnostic point at anything.

**Only the first parse error escapes.** The parser collects every error it found, but
`parse_program_frontend` returns the first one and drops the rest. If you want
multi-error reporting, the errors are already there; the entry point is what limits
it.

## Invariants

1. **The token vector covers the source completely.** Trivia is tokenised, not
   skipped. If lexing starts dropping bytes, the CST stops being lossless and spans
   stop lining up.
2. **The unparser and the parser share one precedence table.** If you add an
   operator, add it to `precedence.rs` once, not to each side.
3. **Code generation leaves both stacks empty.** A non-empty value stack or scope
   stack at the end of `do_compile` panics.
4. **Every construct the parser accepts has a decompiler arm and an unparser arm.**
   A missing arm is not caught at compile time; it is caught when an author reads
   their verb back.
5. **Line numbers in a `Program` refer to unparsed output.** Anything that changes
   how the unparser lays out statements changes traceback line numbers.
6. **Lowering is the only stage that reads `CompileOptions`,** except for
   `call_unsupported_builtins`, which `backend/expr_codegen.rs` reads. Put a new
   feature gate in one of those two places, not in the lexer or the parser.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| `verb_code()` raises `E_INVARG` and a warning names decompilation | The decompiler met an opcode pattern it cannot reverse | Find the opcode in `decompile/mod.rs`. The program is intact; the reader is incomplete. |
| `verb_code()` raises `E_INVARG` and the warning names unparsing | The AST contains a node the unparser cannot print | Add the case in `unparse/`. |
| An author's comments and blank lines disappear after programming a verb | Expected. Only the AST survives; the CST is not persisted | Explain, do not "fix" per verb. A real fix stores or re-derives source. |
| Traceback line numbers do not match the file | Line numbers are unparsed-output line numbers | See above. Compare against `verb_code()` output, not the file. |
| A proptest round-trip test fails | Parse and unparse disagree | Read the saved case in `crates/compiler/proptest-failures/`. Usually a precedence or parenthesisation gap. |
| Code generation panics with "Stack is not empty" | Codegen bug in a new construct | The emitting arm pushed without popping. Not a user error; do not convert it to a `CompileError`. |
| A compile error points at the wrong place | The `ParseErrorDetails` span was not set | Fill in the span where the error is pushed, in `frontend/cursor.rs` callers. |

## Sharp edges worth knowing before you touch the parser

`crates/compiler/src/tests/frontend_sharp_edges.rs` is the accumulated list of
parser cases that were once wrong. Read it before you change tokenisation or
statement dispatch. Several constructs are contextual rather than keyword-driven:
`let`, `const`, `global` and `fn` are only statements when the following tokens look
like a declaration, and flyweight brackets interact with comparison operators, which
is why the parser tracks a flyweight depth and pending closers.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/compiler/src/lexer.rs` | Tokens, spans, keyword and type-constant recognition |
| `crates/compiler/src/syntax_kind.rs` | The token and node kind enum, and `is_trivia` |
| `crates/compiler/src/frontend/syntax.rs` | The rowan language binding and the green-tree builder |
| `crates/compiler/src/frontend/cursor.rs` | Token cursor, trivia skipping, error collection |
| `crates/compiler/src/frontend/parser.rs` | The handwritten recursive-descent parser |
| `crates/compiler/src/frontend/cst.rs` | Typed accessors over the green tree |
| `crates/compiler/src/frontend/lower.rs` | CST to AST, scope resolution, feature gates |
| `crates/compiler/src/ast.rs` | The AST, and tree comparison used by tests |
| `crates/compiler/src/var_scope.rs` | Lexical scopes, declarations, registers, binding to `Names` |
| `crates/compiler/src/codegen.rs`, `backend/` | Code generation |
| `crates/compiler/src/decompile/mod.rs` | `Program` to AST |
| `crates/compiler/src/unparse/` | AST to source, and value literals |
| `crates/compiler/src/precedence.rs` | The shared precedence table |
| `crates/compiler/src/diagnostics.rs` | Error rendering |

## Read first / read next

Read `value-model` before lowering or the unparser, because both handle literals.
Read `program-and-opcodes` before code generation or the decompiler, because both
speak in `Op`, `Name`, `Label` and `Offset`.

After this, read `moor/execution/virtual-machine` for what runs the output, and
`moor/working-in-the-repo/testing` for how language behaviour is asserted.
