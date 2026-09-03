---
name = "Staging a WIT contract change"
brief = "Change wit/thetis.wit without breaking every guest: host impls first, restart, then the contract."
when_to_use = "Use when adding or changing an interface, record or function in wit/thetis.wit. Also use when a guest starts failing at instantiation rather than at compile time, which is the signature of a contract mismatch."
tags = ["wit", "contract", "self-mod", "ordering", "tool-group:selfmod", "tool-group:shell"]
children = "none"
version = 2
---
# Staging a WIT contract change

The WIT contract is shared by the orchestrator and every guest. A guest is
matched against it structurally at instantiation, so a mismatch is not a compile
error — it is a component that builds fine and then refuses to load.

## Why ordering matters

Editing `wit/thetis.wit` triggers a rebuild of every guest that watches it. If
the contract gains a function the host does not yet implement, each guest
rebuilds successfully and then fails to instantiate, because the host cannot
satisfy the import. You lose the agent and the gateway at the same time, which
is precisely when you need them.

So the host must be able to satisfy the new contract *before* the contract
mentions it.

## The order

1. **Write the host implementation first**, against the old contract. It is dead
   code the compiler accepts: nothing calls it yet.
2. **Build the native binary.** `cargo build -p thetis`. A clean build here
   means the host is ready.
3. **Restart the orchestrator.** The running process still has the old code; the
   new implementation only exists on disk until it restarts.
4. **Now edit `wit/thetis.wit`.** Guests rebuild against a host that already
   answers.
5. **Confirm each guest reloaded**, rather than assuming it did.

Doing steps 4 and 1 in the other order gives you a window where nothing
instantiates.

## Removing from the contract

Removal reverses the risk: a guest calling a function the contract no longer
declares will not build.

1. Stop calling it in every guest, and confirm those builds.
2. Then remove it from the WIT.
3. Then delete the host implementation.

An intermediate step that returns an error — a shim — is worth keeping for one
revision, so an in-flight guest gets a clear message instead of a link failure.

## The escape hatch

Before starting, know what you will do if both the agent and the gateway stop
loading: the `/admin` endpoint and a known-good revision number. A contract
change with no rehearsed recovery is the one class of edit that can leave you
unable to edit.
