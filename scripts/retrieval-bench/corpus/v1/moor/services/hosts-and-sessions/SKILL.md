---
name = "mooR hosts and the session abstraction"
brief = "What the telnet host, web host and single-process binary each own, and how a Session ties a connection object to a player and keeps output ordered."
when_to_use = "Use when working on a connection endpoint or on player output, login, attach or reattach, or output that arrives out of order, twice, or not at all. Not for the RPC transport and tokens themselves (read daemon-and-rpc), not for the FlatBuffer definitions (read wire-schema), and not for scrollback and encrypted history (read event-log-and-history) or Thetis internals."
universal = false
tags = ["moor", "moor-telnet-host", "moor-web-host", "telnet-host", "web-host", "session", "websocket", "webrtc", "oauth2", "tls", "connection object", "player object", "switch_player", "connection attributes", "content types", "attach", "detach", "listeners", "output ordering", "moor-server", "single process"]
version = 2
---
