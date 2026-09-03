---
name = "Where to find things in the source"
brief = "A map of the Thetis repository: which orchestrator module owns which behaviour, the guest source trees, and what the dev kit refuses to let you write."
when_to_use = "Use when you must find the file that owns a behaviour, before you read or patch anything: the host imports, the build pipeline, the branch machinery, the prompt cache, the store, the web layer, the terminal, or a guest source tree. Use it also to check whether a path is writable by the dev kit. Not a substitute for list_code and read_code, which give the current truth."
universal = false
tags = ["source map", "repository layout", "files", "modules", "orchestrator", "crates", "where is", "host_api", "pipeline", "tool-group:selfmod"]
related = ["careful-surgery"]
version = 4
---
