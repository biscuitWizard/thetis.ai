---
name = "mooR processes, RPC and hosts"
brief = "The mooR process topology: what the daemon owns, what a telnet or web host owns, and which child skill covers the RPC, the schema, workers, history or MCP."
when_to_use = "Use when the task crosses a process boundary in mooR: deciding whether work belongs in the daemon or a host, or tracing how an event reaches a player. Then read the child skill it points at. Not for MOO verb code inside a game database, not for the object store or the VM (read moor/storage-and-state or moor/execution), and not for Thetis's own internals."
universal = false
tags = ["moor", "daemon", "rpc", "zeromq", "flatbuffers", "telnet-host", "web-host", "workers", "event log", "mcp", "processes", "architecture", "moor-daemon", "moor-telnet-host", "moor-web-host", "curl-worker", "file-worker", "moor-mcp-host", "curve", "enrollment", "paseto"]
children = "auto"
version = 2
---
