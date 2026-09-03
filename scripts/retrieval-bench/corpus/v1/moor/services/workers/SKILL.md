---
name = "mooR out-of-process workers"
brief = "Why blocking or untrusted work runs outside the daemon in mooR, how worker_request() dispatches to a worker process, and how to write a new one."
when_to_use = "Use when MOO code must reach something outside the database — outbound HTTP, files, or any new capability — or a task hangs or a worker never answers. Not for the RPC between hosts and the daemon (read daemon-and-rpc), and not for the FlatBuffer message definitions (read wire-schema) or scheduler internals (read moor/execution/task-scheduler)."
universal = false
tags = ["moor", "workers", "worker_request", "moor-curl-worker", "moor-file-worker", "moor-runtime-api", "moor-zmq-client", "attach", "ping", "detach", "e_invarg", "outbound http", "filesystem", "capability", "sandbox", "python worker", "zeromq"]
version = 2
---
