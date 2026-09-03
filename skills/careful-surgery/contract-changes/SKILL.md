---
name = "Staging a WIT contract change"
brief = "Change wit/thetis.wit without breaking every guest: host impls first, restart, then the contract."
when_to_use = "Use when adding or changing an interface, record or function in wit/thetis.wit. Also use when a guest starts failing at instantiation rather than at compile time, which is the signature of a contract mismatch, or when a new host call returns 'unknown store method' from a tree where the arm is plainly present."
tags = ["wit", "contract", "self-mod", "ordering", "tool-group:selfmod", "tool-group:shell"]
children = "none"
version = 3
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

## Which process actually answers the call

Restarting is necessary but not always sufficient, because
`restart_orchestrator` restarts **your worker only**. Some host calls are not
served by the worker at all:

| The host code you added | Runs in | Live after your restart? |
|---|---|---|
| An `impl <iface>::Host for HostState` method | your worker | yes |
| The linker registration in `runtime.rs` | your worker | yes |
| A `store.*` arm in `persist::serve_store_call` | the **gateway** | **no — needs a merge** |

`Persist::Remote` sends every `store.*` method over IPC to the gateway process,
which is running *trunk's* binary. So a new arm you just wrote compiles, passes
its unit tests, is present in your tree — and still comes back as:

```
unknown store method store.conversations
```

That is not a bug in the arm. It is the same boundary that makes a UI change
invisible until merged, and the fix is the same: merge, or accept that the
end-to-end path cannot be exercised from the branch that adds it.

Two consequences for how you verify:

- **Do not debug the arm** when you see `unknown store method`. Check
  `ps -o lstart=,args= -p $(pgrep -d, -f thetis)` first: if the gateway's start
  time predates your build, that message is expected.
- **Test across the wire instead.** `persist.rs`'s test module pairs a
  `GatewaySide` handler with a `Persist::Remote` over a `UnixStream::pair()`,
  which exercises exactly the path that fails live — serialisation of the params
  and the return value included. A test through `Persist::Local` proves nothing
  about it.

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
