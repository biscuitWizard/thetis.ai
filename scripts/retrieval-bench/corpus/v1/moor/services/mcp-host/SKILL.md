---
name = "The mooR MCP host"
brief = "What moor-mcp-host exposes to an AI assistant, how it authenticates as an ordinary MOO player, and where its safety boundary actually sits."
when_to_use = "Use when connecting an AI assistant to a running MOO, or auditing what it can reach. Use it when a tool returns a permission error or the connection goes stale. Not for the RPC layer it sits on (read daemon-and-rpc), and not for what a MOO permission actually allows (read moor/execution/permissions-and-security) or out-of-process workers (read workers)."
universal = false
tags = ["moor", "mcp", "model context protocol", "ai assistant", "moor-mcp-host", "moor-web-mcp", "stdio", "wizard", "programmer", "moo_*", "resources", "prompts", "credentials", "tools", "safety"]
version = 2
---
