---
name = "mooR out-of-process workers"
brief = "Why blocking or untrusted work runs outside the daemon in mooR, how worker_request() dispatches to moor-curl-worker or moor-file-worker, and how to write a new worker."
when_to_use = "Use when MOO code must reach something outside the database: outbound HTTP, files, or any new capability. Use it when working on moor-curl-worker, moor-file-worker, the worker protocol in moor-runtime-api and moor-zmq-client, the worker_request() builtin, worker attach/ping/detach, or when a task hangs, returns E_INVARG for no worker available, or a worker never answers. Also use it to write a worker in another language. Not for the RPC between hosts and the daemon (read daemon-and-rpc), not for the FlatBuffer message definitions (read wire-schema), not for scheduler internals (read moor/execution/task-scheduler), and not for MOO verb code inside a running world such as the Torchship database, which the torchship skills own, or for Thetis internals."
universal = false
tags = ["moor", "workers", "worker_request", "curl-worker", "file-worker", "outbound http", "filesystem", "capability", "sandbox", "python worker", "zeromq"]
version = 1
---

# mooR out-of-process workers

A worker is one capability the daemon deliberately refuses to have. MOO code asks
for it with `worker_request()`, the task suspends, a separate process does the
work, and the answer wakes the task. Workers are how mooR keeps "the database
server" and "the thing that talks to the internet" in different processes with
different privileges.

## Why not just do it in the daemon

Three reasons, in order of weight.

1. **Blocking.** The daemon runs the scheduler and the VM. An outbound HTTP call
   or a filesystem read can take seconds or never return. Anything that blocks
   inside the daemon blocks tasks that have nothing to do with it.
2. **Privilege.** The daemon holds the world database and the signing keys. It
   should not also hold network egress and filesystem write. A worker can run as
   a different OS user, in a different container, or on a different machine with
   a different firewall policy.
3. **Refusability.** An operator must be able to say no. A capability that lives
   in a separate binary is off by default simply by not being started. The file
   worker is the clearest case: it is not embedded in the single-process `moor`
   binary and a normal installation does not run it.

The corollary is a rule: **never add an outbound network call or a filesystem
call to the daemon on behalf of MOO code.** If you want a new capability, you
want a new worker.

## The shape of the protocol

Two ZeroMQ sockets, mirroring the host protocol.

| Direction | Pattern | Endpoint (daemon side) | Carries |
|---|---|---|---|
| Daemon to worker | PUB/SUB on the worker broadcast topic | `--workers-request-listen` | Ping, work requests, "please die" |
| Worker to daemon | REQ/REP | `--workers-response-listen` | Attach, pong, result, error, detach |

Security follows the endpoint scheme exactly as it does for hosts: `tcp://` gets
CURVE and requires enrollment, anything else does not. See `daemon-and-rpc`.

The life of a request:

1. MOO code calls `worker_request(worker_type, args [, options])`. It is
   wizard-only.
2. The VM returns a suspend instruction. The scheduler **commits the task's
   world-state transaction**, then mints a request UUID and hands the request to
   the workers server, and parks the task on that UUID.
3. The daemon picks an attached worker whose type symbol matches, preferring the
   one with the fewest queued requests. If there is none, the task is failed
   immediately with a no-worker-available error.
4. The request is published on the shared worker topic. It carries the target
   worker UUID, the request UUID, the authority principal, the argument list and
   a timeout in milliseconds (zero meaning none).
5. Every subscribed worker receives it; each discards anything not addressed to
   its own UUID.
6. The worker does the work and sends a result or an error back over REQ/REP.
7. The daemon wakes the parked task with the result, in a **new** transaction.

Workers are pinged on the same cadence as hosts. A worker that misses the worker
timeout is dropped from the registry and every request it had outstanding is
failed with a worker-detached error.

## What a worker may assume about the requesting task

Very little, and this is the part people get wrong.

- **The task's transaction has already committed.** The worker is not inside it
  and cannot be. Whatever the worker does is not atomic with the MOO world, and
  cannot be rolled back by aborting the task. This is stated plainly for the file
  worker in the book, and it is true of every worker.
- **The task may be gone by the time the answer arrives.** It may have been
  killed, or the server may have restarted. A worker must not assume its reply
  will be consumed.
- **The `authority_principal` is context, not authority.** It is the object the
  calling task's permissions derive from. The daemon does not use it to authorise
  the worker; the worker may use it to log or to make its own policy decision.
  The real gate already happened: `worker_request()` is wizard-only.
- **The timeout is advisory and the worker enforces it.** The daemon parks the
  task on a wake condition that has no deadline. If a worker accepts a request
  and never answers, and keeps answering pings, the task waits forever.
- **The request is visible to every attached worker.** Dispatch is a broadcast
  with an addressee, not a point-to-point send. Do not put a secret in a worker
  request and assume only the target sees it.

## The workers that exist

| Worker | Type symbol | Capability | Notes |
|---|---|---|---|
| `crates/curl-worker` | `curl` | Outbound HTTP | Embeddable in the single-process binary; the usual default worker |
| `crates/file-worker` | `file` | Reads and writes inside one directory | Opt-in only. Requires `--sandbox-dir` and never embeds in `moor` |
| `tools/example-python-worker` | `echo` in the sample | Nothing; it is a protocol proof | Shows that the protocol is language-neutral |

The file worker's sandbox is worth studying as the model for any worker that
touches the host. It opens the sandbox root once as a directory capability and
resolves every request path through it, so lookup stays confined even if
directories or symlinks change underneath. Its error messages mention only the
caller's relative path, so host layout does not leak back to MOO code.

## Writing a new worker

1. **Decide the type symbol.** It is the first argument to `worker_request()` and
   the only routing key. Pick a short noun.
2. **Decide the request and reply shape.** Both are lists of MOO `Var`. Keep them
   simple and self-describing; there is no schema for a worker's payload, so the
   contract is between the worker and the MOO code that calls it.
3. **Take the standard connection arguments.** Reuse `RpcClientArgs` from
   `moor-runtime-api` so your worker accepts the same addresses, data directory
   and enrollment token file as everything else.
4. **Call `setup_curve_auth`** with your service type string. It returns no keys
   for an IPC address and enrols for a TCP one.
5. **Write the perform function**: request id, worker type, authority principal,
   arguments, optional timeout, returning a `Var` or a `WorkerError`. Enforce the
   timeout yourself.
6. **Run `worker_loop`** from `moor-zmq-client` with a fresh worker UUID. It
   handles attach, ping, addressing, decoding and replying.
7. **Add a health endpoint** if the deployment expects one. The existing workers
   expose a trivial TCP port that reports unhealthy once the daemon's pings stop.
8. **Choose the right `WorkerError`.** The variants are the vocabulary MOO code
   sees: permission denied, invalid request, internal error, request timed out,
   request error, worker detached, no worker available. Do not collapse
   everything into an internal error.
9. **Document the capability and its risk**, the way `book/src/the-system/file-worker.md`
   does. A worker is an operator decision, so an operator needs to be able to
   make it.

To write a worker in another language, generate bindings from the `.fbs` files
(see `wire-schema`) and implement the same two sockets. The Python example is the
working reference, including its regeneration command.

## Invariants

1. **The daemon performs no outbound I/O for MOO code.** Every such capability is
   a worker.
2. **A worker request crosses a transaction boundary.** The task committed before
   the request went out and resumes in a new transaction. Nothing the worker does
   is atomic with the world.
3. **A worker answers with exactly one result or one error per request id.** The
   daemon matches on that id; a second answer has nowhere to go.
4. **A worker enforces its own timeout.** The scheduler will not rescue it.
5. **`worker_request()` is wizard-only.** Do not add a path that reaches a worker
   without that check.
6. **A worker ignores requests not addressed to its own UUID.** Dispatch is a
   broadcast.
7. **A worker confines its capability itself.** The daemon passes no policy. The
   file worker's sandbox root is the pattern.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/kernel/src/vm/builtins/bf_connection.rs` | The `worker_request()` builtin: wizard check, option parsing, suspend |
| `crates/kernel/src/tasks/scheduler/scheduler_task_callbacks.rs` | Turning the suspend into a request and parking the task |
| `crates/kernel/src/tasks/workers.rs` | The scheduler-side request and response types |
| `crates/daemon/src/workers/message_handler.rs` | The worker registry, selection, dispatch, ping and expiry |
| `crates/daemon/src/workers/server.rs`, `transport.rs` | The workers REP socket and its loop |
| `crates/runtime-api/src/worker.rs`, `worker_messages.rs` | The typed worker vocabulary and message builders |
| `crates/zmq-client/src/worker_loop.rs`, `worker_rpc_client.rs` | The reusable worker main loop |
| `crates/curl-worker/`, `crates/file-worker/` | The two shipped workers |
| `tools/example-python-worker/` | Cross-language reference implementation |
| `book/src/the-system/file-worker.md` | The operator-facing case for why this is opt-in |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| `worker_request()` fails immediately, no worker available | No worker of that type is attached | Start the worker; check its type symbol matches the call exactly |
| The whole task aborts with no useful error | The daemon was started with workers disabled, so there is no worker channel at all | Enable the worker transport on the daemon |
| The task suspends and never returns | The worker accepted the request, is still answering pings, and never replied | Look at the worker's own logs. The scheduler has no deadline for this; kill the task |
| Tasks fail with worker detached | The worker missed the ping timeout, or exited | Every request it held was failed. Restart it; consider running more than one worker of that type |
| Requests reach the wrong worker | Two workers share a type symbol, which is legal, and the work is not idempotent | Selection is by queue depth, not by affinity. Do not assume one worker |
| Worker connects but receives nothing over TCP | CURVE key not enrolled | See the enrollment failure table in `daemon-and-rpc` |
| Worker arguments look swapped in the command line | The help text on `--workers-request-address` and `--workers-response-address` in `RpcClientArgs` describes them the wrong way round | Trust the daemon's own `--workers-request-listen` (PUB, daemon to worker) and `--workers-response-listen` (REP, worker to daemon) |
| File worker rejects a path | The path escaped the sandbox root | Correct behaviour. Move the file inside the sandbox; do not widen the root to the database directory |

## Read first / read next

- Read `moor/execution/task-scheduler` for suspension and wake conditions; a
  worker request is one wake condition among several.
- Read `moor/storage-and-state/transactions` to understand what "the transaction
  already committed" costs you.
- Read `moor/execution/permissions-and-security` for the wizard check.
- Read `daemon-and-rpc` for enrollment and CURVE, which are shared with hosts.
