---
name = "Staging a WIT contract change"
brief = "Change wit/thetis.wit without breaking every guest: host impls first, restart, then the contract."
when_to_use = "Use when adding or changing an interface, record or function in wit/thetis.wit. Also use when a guest starts failing at instantiation rather than at compile time, which is the signature of a contract mismatch."
tags = ["wit", "contract", "self-mod", "ordering", "tool-group:selfmod", "tool-group:shell"]
children = "none"
version = 2
---
