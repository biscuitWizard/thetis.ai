---
name = "Cores, bootstrap, and what the server needs from a database"
brief = "Pick a bundled mooR core, bring a world up from nothing, and know which objects, properties and verbs the server itself looks for — and what breaks when they are absent."
when_to_use = "Use when starting a mooR world for the first time, choosing between cowbell, lambda-moor, minimal-core and the benches core, running start-moor-cowbell.sh or start-moor-lambdacore.sh, reading docker-compose.yml, process-compose.yaml or moor-dev.yaml, rebuilding a core with its Makefile and moorc, or configuring checkpoint exports. Use it above all when the server runs but a connection cannot log in, or a hook such as do_login_command, user_connected, server_started, do_command, huh, handle_uncaught_error or $server_options is missing or ignored. Not for objdef file syntax (read objdef-format), not for importing a LambdaMOO textdump (read textdump-compat), not for the Torchship game database or any specific world's own objects, not for in-world verb authoring for a specific game, and not for Thetis's own internals."
universal = false
tags = ["moor", "core", "cowbell", "lambda-moor", "lambdacore", "minimal-core", "bootstrap", "first start", "do_login_command", "server_options", "server_started", "user_connected", "checkpoint", "docker-compose", "process-compose", "moorc", "login fails", "dump_interval"]
related = ["moor/working-in-the-repo/build-and-run", "moor/services/daemon-and-rpc"]
version = 1
---

# Cores, bootstrap, and what the server needs from a database

This skill is written in ASD-STE100 Simplified Technical English.

mooR is an engine with no world in it. A **core** is the starter database that makes
the engine usable: a system object, a root object, players, rooms, a login path, and
the library code above them. This page covers the cores the repository ships, how a
world comes up from nothing, and the small set of things the server itself reaches
into the database to find.

## What a core is, and is not

The engine knows about objects, properties, verbs, permissions and tasks. It knows
almost nothing about rooms, players, commands or logins. Everything a person
experiences is written in MOO and lives in the database.

So a core is not configuration and not a template. It is the world, and after the
first start it is the live database. The source directory that built it is a
historical record from that moment on. Read `objdef-format` for how the two diverge
and how to move changes between them.

## The bundled cores

| Path | What it is | Start here when |
|---|---|---|
| `cores/cowbell/src` | mooR's own core, written from scratch for the web client. Event driven, rich content, uses mooR-only language features. | You are building a new world, or working on mooR itself. This is the default in `docker-compose.yml` and `moor-dev.yaml`. |
| `cores/lambda-moor/src` | LambdaCore 2018, converted to objdef and edited until it works with mooR's modern flags. Passwords moved to argon2. | You want a familiar LambdaCore environment, or a compatibility test. |
| `cores/minimal-core/src` | Four objects: system object, root, one room, one wizard. Enough to log in and evaluate expressions. | You are testing the runtime, or building a world with no inherited assumptions. |
| `cores/benches/src` | Not a usable world. Objdef files whose `test_` verbs are performance benchmarks. | You are measuring the engine. |
| `cores/JHCore-DEV-2.db` | A JaysHouseCore textdump. It is the fixture for the large textdump import test. | You are testing textdump import. |

Constraints that decide whether a core will even load:

- **Cowbell requires the `main` branch.** Its README says so: it uses post-1.0
  runtime and compiler features and does not work against the 1.0 release branch.
- **Every core declares its feature flags in its own `Makefile`.** Read the `OPTIONS`
  variable there. Cowbell also needs `--anonymous-objects true`; the benches core
  explicitly sets it false. The server side of the same flags is the `features`
  block of the config file, and `FeaturesConfig` in `crates/vm/src/config.rs`.
- **Licensing is not uniform.** Only `minimal-core` is under mooR's own licence.
  `cores/LICENSING.md` explains the position on LambdaCore-derived material.

## Bringing a world up

The rule that surprises everyone first: **an import happens only when the database
did not exist.** The daemon creates the database, and only then reads the import
path. On every later start the import path is ignored. Editing the source directory
does not change an existing world, and neither does restarting.

If the import fails, the daemon deletes the database it had just created and exits
with the error. It removes only that database, not the whole data directory.

| Route | What it does | Use it for |
|---|---|---|
| `scripts/start-moor-cowbell.sh`, `scripts/start-moor-lambdacore.sh` | Set `IMPORT_PATH` and `RUN_DIR`, then run `docker compose up --build`. | The quickest complete stack, with the web client. |
| `docker-compose.yml` | Runs the single-process `moor` binary with `--import`, `--import-format=objdef`, `--export`, and `--generate-keypair`. | Reading exactly which options a first start uses. |
| `process-compose.yaml` | Runs the daemon and each host as separate cargo processes, importing `cores/lambda-moor/src`. | Working on one host process at a time. |
| The `moor` or `moor-daemon` binary directly | `--import`, `--import-format`, `--export`, `--db`, `--config-file`. | Scripted or unusual setups. |
| `moorc` | Compiles a source tree into a database or an objdef dump, with no server. | Validating a core, converting a textdump, running a core's tests. |
| `make -C cores/<core>` | Wraps `moorc` to produce `gen.objdir`; `make test` runs the core's `test_` verbs; `make rebuild` overwrites `src/` with normalised output. | Core development. `rebuild` is destructive; review the diff. |

To start over, delete the database directory and start again. That is the supported
way to re-import.

Exports go the other way. When an export path and a checkpoint interval are
configured, a background thread asks the scheduler for a checkpoint. The checkpoint
takes a read-only snapshot, collects every object, writes an objdef directory named
`checkpoint-<seconds>.in-progress`, and renames it to `checkpoint-<seconds>.moo` as
the last step. `dump_database()` requests one on demand. Checkpoints are always
objdef, whatever `--export-format` says.

## What the server requires of the database

This is the part that decides whether a core works. The engine reaches into the
database in a small number of named places. Each one is defined in code as a symbol
constant, and each list changes, so read the constant, not a copy of it.

**Settings the server reads.** `reload_server_options` in
`crates/kernel/src/tasks/scheduler/scheduler_config.rs` holds the whole set as
`LazyLock<Symbol>` constants at the top of that file. Two facts about the shape:

- Most are read from the object that `#0.server_options` points at. If that property
  is missing, or is not an object, every setting keeps its default and the server
  logs that it is using defaults.
- Some are read from `#0` itself, not from `$server_options`. `dump_interval` and
  `gc_interval` are on `#0`. The book's table omits `gc_interval`.
- A value of the wrong type produces a warning and the default. Nothing here is
  fatal, and nothing here is required.

**Verbs the server calls.** Each name below is a symbol constant in the file named.
Missing verbs are not startup errors; the effect appears when the situation arises.

| Verb | Called on | Defined in | Effect when absent |
|---|---|---|---|
| `do_login_command` | The listener's handler object, which defaults to `#0` | `crates/daemon/src/rpc/message_handler.rs`, used in `daemon_api_impl.rs` | Nobody can log in. The login task fails and the client gets a login failure. |
| `user_connected`, `user_reconnected`, `user_created`, `user_disconnected` | The handler object | `crates/daemon/src/rpc/message_handler.rs`, used in `message_handler_tasks.rs` | Login still works. The world is never told that a player arrived or left. |
| `server_started` | `#0` | `crates/daemon/src/lib.rs` | Skipped, with a debug log. Anything the world wanted to restart does not restart. |
| `do_command` | `#0` | `crates/kernel/src/tasks/task.rs` | Every command goes straight to the normal command parser. This is a valid design, not an error. |
| `huh` | The player's location | `crates/kernel/src/tasks/task.rs` | An unmatched command reports no match instead of being handled in-world. |
| `handle_uncaught_error` | `#0` | `crates/kernel/src/tasks/task.rs` | An uncaught error is logged by the server and not reported in-world. |
| `handle_task_timeout` | The scheduler's callback path | `crates/kernel/src/tasks/scheduler/scheduler_task_callbacks.rs` | A task that hits a limit is finalised with no in-world notice. |
| `do_out_of_band_command` | The handler object | `crates/kernel/src/tasks/scheduler/scheduler_submit.rs` | Out-of-band input is not handled. |
| `initialize`, `recycle` | The object being created or destroyed | `crates/kernel/src/vm/builtins/bf_objects.rs` | `create()` and `recycle()` still work; the object gets no lifecycle callback. |

**Objects.** `#0` is the system object and is hard-coded as `SYSTEM_OBJECT`
throughout the engine. Everything else is convention. `#1` as a root object and `#2`
as a first room are core conventions, not engine requirements; `minimal-core` shows
the smallest set that works.

**Nothing is validated at startup.** The daemon opens the database, optionally
imports, starts the scheduler, and calls `server_started` if it exists. It never
checks that a login path exists. A core with no `do_login_command` starts cleanly and
accepts connections that can never log in.

## Invariants

1. The import path is read only when the database file was just created.
2. A failed import leaves no database behind, so a retry is always from a clean state.
3. Checkpoints are objdef directories, always, and the `.moo` name appears only after
   a successful write.
4. `#0` is the system object. No other object number is fixed by the engine.
5. A core's feature flags and the server's feature flags must agree. The core's
   `Makefile` is the record of what the core was built with.

## Where the code and the configuration live

| Path | Responsibility |
|---|---|
| `crates/daemon/src/lib.rs` | Opening the database, deciding whether to import, `perform_import`, cleanup on failure, the checkpoint thread, the `server_started` hook. |
| `crates/daemon/src/args.rs` | `--import`, `--export`, `--import-format`, the deprecated `--export-format`, `--checkpoint-interval-seconds`. |
| `crates/server/src/main.rs` | The same options on the single-process `moor` binary. |
| `crates/kernel/src/config.rs` | `ImportExportConfig` and `ImportFormat`, the file-config form of the same options. |
| `crates/kernel/src/tasks/scheduler/scheduler_config.rs` | Every `$server_options` and `#0` setting the server reads. |
| `crates/kernel/src/tasks/checkpoint.rs` | The checkpoint export. |
| `crates/vm/src/config.rs` | `FeaturesConfig`: the flag set and its defaults. |
| `moor-dev.yaml`, `docker-compose.yml`, `process-compose.yaml` | The three worked examples of a first start. |
| `cores/*/Makefile` | The flags each core requires, and its build, test and rebuild targets. |
| `cores/LICENSING.md` | The licence position for each bundled core. |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| Source edits have no effect after a restart | The database already exists, so the import was skipped | Delete the database directory to re-import, or apply the change in-world with `load_object` or `reload_object`. |
| The daemon exits during a first start and no database remains | The import failed and the daemon removed the database it created | The error above the removal message is the real one. Reproduce it with `moorc` for a faster loop. |
| The world starts, a connection is accepted, and login never completes | No `do_login_command` on the handler object, or it does not return an object | Check the handler object of the listener, which defaults to `#0`. A login verb must return a player object. |
| Login works, but the world never reacts to arrivals | No `user_connected` or `user_created` | These are separate from the login verb. Add them. |
| Nothing the world scheduled at boot is running | No `server_started`, or it failed | The hook is optional and its absence is only a debug log. It also has a completion timeout. |
| A core fails to compile with errors about unknown syntax | The core needs feature flags the server does not have | Compare the core's `Makefile` `OPTIONS` with the server's `features` block. Cowbell also requires the `main` branch. |
| `$server_options` settings are ignored | `#0.server_options` is missing, or is not an object | The log says the server is using defaults. It never fails. |
| Checkpoints never appear | No export path, or no checkpoint interval | Both are needed. The error when only the interval is set is logged as an inability to checkpoint. |
| A core `Makefile` target fails on an unknown option | `cores/lambda-moor` still has a `gen.moo-textdump` target, and `moorc` has no textdump output | Use `gen.objdir` instead. |
| Two servers appear to fight over one database directory | The database is opened by one process only | Stop one. `moor-emh` takes an exclusive directory lock for exactly this reason. |

## Recovering a world with no working login

`tools/moor-emh` exists for this. It opens the database directly, with the server
stopped, takes an exclusive lock on the data directory, and finds a wizard by scanning
for the wizard flag and choosing the lowest object number. It gives a REPL that can
evaluate code, read and write properties, program verbs, and `dump`, `load` and
`reload` objdef definitions. Use it to install or repair a login verb, then start the
server again. Read `moor/working-in-the-repo/repo-tooling` for the tool set it
belongs to.

## Read first, read next

Read first:

- `objdef-format` — the format every bundled core is written in.
- `moor/working-in-the-repo/build-and-run` — building the binaries these routes run.

Read next:

- `textdump-compat` — bringing a world in from an older server.
- `moor/services/daemon-and-rpc` — the process that owns the import, the checkpoint
  thread, and the login path.
- `moor/execution/task-scheduler` — what `$server_options` actually controls.
- `book/src/the-system/understanding-moo-cores.md` and `bootstrapping-from-source.md`
  — good background. Check `server-assumptions-about-the-database.md` against
  `scheduler_config.rs`; the book's list is behind.
