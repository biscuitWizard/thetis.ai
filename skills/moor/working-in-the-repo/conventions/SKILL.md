---
name = "mooR contribution rules"
brief = "The rules a mooR patch must satisfy: licence headers, nightly rustfmt and dprint, clippy-clean, dependency and export policy, no legacy shims, and commit and pull-request norms."
when_to_use = "Use before writing or submitting a change to mooR: which formatter to run, where a dependency version goes, why a module is not public, what the licence header must say, how a commit message and pull request should read, and what the project asks an AI coding partner not to do. Use it when a formatting or licence check fails. Do not use it to compile or start a server, or to choose a test; those are build-and-run and testing. Not for the Torchship game database, for authoring MOO verbs inside a running world, or for Thetis's own internals."
universal = false
tags = ["moor", "conventions", "style", "rustfmt", "dprint", "clippy", "licence header", "licensure", "dependencies", "commit message", "pull request", "code review", "agents.md"]
related = ["moor/storage-and-state/storage-engine"]
version = 1
---

# mooR contribution rules

Some of these are enforced by a tool and will stop a pull request. Others are
judgement the project states plainly and reviews for. Both kinds are listed, and
each is given with its reason, because a rule whose reason you know is one you
can apply to a case nobody wrote down.

`AGENTS.md` and `CONTRIBUTING.md` in the repository are the source. This skill
reproduces their intent and notes where a detail has drifted.

## The enforced gates

CI fails on any of these. Run each before you submit.

| Gate | Command | Notes |
|---|---|---|
| Build | `cargo build --workspace --exclude lambdamoo-harness --all-features --all-targets` | CI builds all features and all targets, so benches and tests compile too |
| Tests | `cargo test --workspace --exclude lambdamoo-harness`, then the documentation tests | See `testing` |
| Rust formatting | `scripts/format-rust.sh`, or `--check` for the check | Nightly rustfmt, with three import settings |
| Clippy | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Every warning is an error |
| Other formatting | `npx dprint fmt`, or `npm run format:check` | JSON, TOML, Markdown, TypeScript and Dockerfiles |
| Licence headers | `licensure --check -p`, or `scripts/licensure-project.sh --check` | Install `licensure` with cargo |

### Why nightly rustfmt

The project pins three import options that are not on stable: reordered
imports, block indentation for imports, and a mixed import layout. This is the
only nightly use in the project; nothing else compiles on nightly. Running plain
`cargo fmt` on stable will produce a diff that the check job rejects, because
your stable rustfmt does not know those options and formats imports differently.
Always go through `scripts/format-rust.sh`, which supplies them.

### The licence header

Every source file carries a licence header. `.licensure.yml` decides which
licence, by path:

| Path | Licence in the header |
|---|---|
| Everything by default | AGPL-3.0 |
| `crates/schema/schema/` (the `.fbs` files) | LGPL-3.0-or-later |
| `clients/web-sdk/` | LGPL-3.0-or-later |
| `clients/meadow/` | GPL-3.0 |

The differences are deliberate. The server is AGPL so that a network deployment
stays reciprocal. The schema and the browser SDK are LGPL so that a third-party
client can link them. Do not change a header to match a neighbouring file
without checking which rule applies.

`AGENTS.md` says GPLv3. The tool says AGPL-3.0 for the default case, and the
workspace licence field agrees. Follow the tool.

The wrapper `scripts/licensure-project.sh` runs licensure over the files git
tracks, rather than over the whole working tree. Use it so that build output and
untracked scratch files are not touched.

## Dependency policy

1. **Every third-party version is declared once,** in `[workspace.dependencies]`
   in the root `Cargo.toml`. A member crate writes `.workspace = true` and no
   version. The reason is that one workspace-wide version prevents two crates in
   the same process from linking two copies of the same library.
2. **Prefer a dependency with a small transitive tree.** The project states this
   directly. A crate that pulls in dozens of others costs build time for every
   contributor and widens the licence and supply-chain surface.
3. **Do not violate the AGPL, in letter or in spirit.** A dependency whose
   licence is incompatible cannot go in.
4. **Adding a dependency is a decision, not a detail.** Propose it. The
   workspace already patches two dependencies to forks; that is the level of
   care the project takes with them.
5. **Avoid async and tokio unless the crate you are working in already uses
   them.** The core runtime crates are synchronous and threaded on purpose. The
   hosts are async because network servers are. Do not carry async into a crate
   that has none.

## Structural rules

**Export from `lib.rs`.** A crate makes its public surface available from its
own `lib.rs`. Downstream crates use that surface and do not reach down into
modules. The reason is that the module tree is then free to change: an internal
reorganisation stays internal. When you need something from another crate and it
is not exported, the correct fix is to export it there, not to make its module
public and reach in.

**No legacy-compatibility scaffolding.** The project has no installed base to
protect. Do not write migration paths, shims, compatibility bridges, or code
whose comment says it supports an older way. Change the thing and delete the old
one. This is stated as an anti-pattern in `AGENTS.md`. It does not apply to
LambdaMOO compatibility, which is a product requirement, not scaffolding: mooR
aims to behave like LambdaMOO 1.8.x and 1.9.x, and includes some ToastStunt
behaviours without aiming for full ToastStunt compliance.

**Name what the code does, not how it does it or when it was written.** No
`NewApi`, no `LegacyHandler`, no name that encodes a library or a pattern unless
the pattern is the clearest description.

## Rust style

The project's stated preferences, each with its reason.

| Rule | Reason |
|---|---|
| Early returns; handle the error and invalid cases first | Rust code that matches over algebraic types nests deeply and becomes unreadable. Leaving the happy path last and unindented shows the function's purpose |
| Prefer `let ... else` over nested conditional binding | Same reason. It removes a level |
| Avoid `else` branches after an `if` that returns | Same reason |
| Factor a deep block into a named function | Same reason, when the first three are not enough |
| All `use` statements at the top of the file or module | Per-function imports hide what a file depends on. The project names this as a habit it dislikes in machine-written code |
| Put variables inline in format strings | Shorter and reads better |
| Avoid a strongly object-oriented style | The codebase is data-oriented; layered abstractions cost indirection in hot paths |
| Small functions, and a comment where control flow is not obvious | Readability is a primary concern, stated as such |

### Performance

Performance is stated as paramount, especially in `crates/kernel` and
`crates/db`. In practice that reaches `crates/var` and `crates/vm` too, because
they are on every instruction's path. Prefer a zero-copy or low-copy design,
follow cache-friendly access patterns, and avoid allocation in a hot path.

The rule that decides a review: **an optimisation must be proved, not
asserted.** A change presented as faster comes with a before and an after. An
unmeasured optimisation is not acceptable, and neither is a readability
sacrifice with no number behind it. `performance-and-profiling` owns how to
produce that evidence, and what these words mean concretely in each hot crate.

### Comments and documentation

- Say **why** and **what**, not how, unless the how is genuinely surprising.
- Keep comments evergreen. No comment that refers to a previous way of doing
  something, and no comment that dates itself.
- Major functions and modules carry Rustdoc.
- No laudatory or advertising language anywhere: not in code, not in comments,
  not in documentation, not in a reply. "Comprehensive" is named as forbidden.
  Do not claim anything is "production ready".

### TypeScript and other files

Four-space indentation. ESLint configuration lives with each package, as does
each package's own `lint` and `typecheck` script; the root exposes a combined
type-check and a formatting check. Everything that dprint handles — JSON, TOML,
Markdown, TypeScript, Dockerfiles — is formatted by dprint, with `cores/` and
vendored trees excluded.

## Documentation duty

`book/` is the user documentation, built with mdBook. A change to user-visible
behaviour updates the book **in the same pull request**. This is stated twice in
`CONTRIBUTING.md`, so treat it as firm. A book page that is wrong is a bug worth
its own pull request.

Where the book and the code disagree, the code is right. Say so, and fix the
book.

## Commits and pull requests

**Commits.** Short imperative subject line, describing the change. A body when
the change is cross-cutting. Squash incidental formatting into the main commit
so the diff is not noise.

**Pull requests.** One unit of work: a single feature, or a single bug.
Several commits are fine as long as they serve that one problem. The description
should state the problem, the approach, and any data or migration impact; link
the issue; attach a screenshot or terminal capture for anything user-visible;
and list the commands you ran.

**Keep artefacts out of the diff.** Generated exports, database directories and
signing keys must not appear unless the rotation is the point of the change.

**Branches.** The project runs a development line and a stable release line, and
bug fixes land on development first and are back-ported when safe. The exact
branch names have changed during the 2.0 cycle and the release tooling and
`CONTRIBUTING.md` do not use the same name. Read `CONTRIBUTING.md` and the
release workflow rather than trusting a name written here, and ask if it
matters.

## What the project asks of an AI coding partner

`AGENTS.md` and the LLM section of `CONTRIBUTING.md` state these.

1. **Ask before a major change.** Augmentation, not autonomy. The human partner
   decides. Propose, agree, then write.
2. **Do not run git commands unless explicitly asked.** No commit, no branch, no
   push, no rebase on your own initiative.
3. **Write no marketing language.** See above; this is repeated in three places
   in the project's own documents.
4. **Do not generate the commit message or the pull-request description.** The
   human writes those, in their own words.
5. **Claim nothing you did not verify.** No invented technical detail, no
   plausible-sounding behaviour you did not check.
6. **Expect the work to be reviewed as the human's own.** They are responsible
   for it, so they must be able to explain every line. Write code that can be
   explained.
7. **Confused, over-complicated or poorly reasoned code is called out by name in
   this project.** Simpler is the house preference.

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| The format check fails on imports you did not touch | You ran stable `cargo fmt` | Run `scripts/format-rust.sh`. Stable rustfmt reorders imports differently from the pinned nightly settings |
| The format check fails on Markdown, JSON or TOML | dprint was not run | Run `npx dprint fmt`. It also enforces line widths, so a hand-wrapped Markdown paragraph will be rewrapped |
| The licence check fails on a file you added | No header, or the wrong licence for that path | Run licensure in write mode through the wrapper script, and check the path rules above |
| The licence check touches files you did not add | You ran licensure over the whole tree | Use `scripts/licensure-project.sh`, which limits itself to tracked files |
| Clippy passes locally and fails in CI | You did not pass `--all-targets --all-features` | Benches and test code are linted too. Use the full command |
| A reviewer asks for the number behind an optimisation | The project requires evidence for performance claims | Produce a before and after from the same machine, with release builds. See `performance-and-profiling` |
| You cannot reach a type in another crate | The crate exports from `lib.rs` and that type is not exported | Export it there. Do not make the module public to reach in |
| A change touches many unrelated files | Formatting or a rename leaked into the change | Split it. One pull request, one problem |

## Read first / read next

Read `AGENTS.md` and `CONTRIBUTING.md` in the repository; this skill is their
summary, not their replacement. Read `testing` for the evidence a change needs,
and `repo-tooling` for how to install and run licensure, dprint and the book
tools.
