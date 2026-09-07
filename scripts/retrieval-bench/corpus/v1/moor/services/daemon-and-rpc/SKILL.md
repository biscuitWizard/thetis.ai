---
name = "The mooR daemon and its RPC layer"
brief = "How moor-daemon serves hosts and workers over ZeroMQ: request/reply versus pub/sub, CURVE enrollment, PASETO tokens, and how an event reaches a player."
when_to_use = "Use when working on the daemon or the RPC between processes: an RPC message, host registration, or enrollment. Use it when a host cannot enroll, an event never arrives, or a client is dropped for a backlog. Not for the FlatBuffer definitions (read wire-schema), and not for what a host does with an event (read hosts-and-sessions) or worker dispatch (read workers)."
universal = false
tags = ["moor", "daemon", "rpc", "zeromq", "curve", "zap authentication", "allowed-hosts", "paseto", "clienttoken", "authtoken", "enrollment", "pub-sub", "client token", "auth token", "listeners", "moor-daemon", "sequence numbers", "ping timeout"]
version = 2
---
