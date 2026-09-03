---
name = "Multi-user Thetis"
brief = "Configure and maintain Thetis web accounts, roles, ownership, and account policy."
when_to_use = "Use when enabling auth.mode=users, adding accounts or roles, diagnosing login/ownership isolation, or changing per-user capability, model, mode, spend, Discord, or tool policy. Not for Discord's separate pairing allow-list."
universal = false
tags = ["thetis", "authentication", "accounts", "roles", "ownership", "policy", "login"]
version = 1
---

# Multi-user Thetis

Keep identity and authorization native. `crates/thetis/src/auth.rs`, `policy.rs`,
`web.rs`, `host_api.rs`, `persist.rs`, and `store.rs` are the trusted boundary;
the WebAssembly gateway and agent only hide or explain restrictions already
published by the host.

## Enable accounts

1. Generate a PHC hash without echoing the password:
   ```sh
   thetis hash-password
   ```
   For a pipe or script, use `thetis hash-password --stdin`.
2. Put roles and users in `thetis.local.toml`, not the tracked example:
   ```toml
   [auth]
   mode = "users"
   claim_unowned = "alice"
   discord_role = "reader"

   [[roles]]
   id = "admin"
   admin = true

   [[roles]]
   id = "reader"
   read_only = true
   deny_capabilities = ["transcripts", "delegation", "workspace_write"]

   [[users]]
   id = "alice"
   name = "Alice"
   role = "admin"
   password_hash = "$argon2id$..."
   ```
3. Keep the listener on loopback unless a TLS reverse proxy is in front. For a
   non-loopback bind, set `server.public_origin` to the browser origin and
   preserve `Host`; Thetis deliberately ignores `X-Forwarded-*`.
4. Restart. Startup rejects duplicate or malformed users, missing roles,
   unresolved models or modes, a missing admin, a bad `claim_unowned`, and a
   non-loopback users-mode bind without `public_origin`.

A user's hash may instead come from `password_env`, or from the convenience
variable `THETIS_USER_<NORMALIZED_ID>_PASSWORD_HASH`. Exactly one source must
resolve.

## Understand ownership

Ownership is stored in the redb `owners` side table rather than `SessionMeta`,
because that record is a WIT contract. New conversations are stamped atomically.
Sub-agents inherit their root conversation's owner. At gateway boot, legacy
unowned conversations are assigned to `local`, or to `auth.claim_unowned` in
users mode.

Every browser and worker path must resolve ownership at the native boundary:
session listing and transcript recall, open/get, mutation, subscription,
preview, branch/debug frames, and user-scoped KV. Never trust a session id
returned by a guest.

## Set policy

Role and user policy layers narrow the global model/mode catalogues. Lists
replace inherited lists rather than merging. `read_only = true` implies denial
of native write capabilities regardless of the selected mode.

Use `deny_capabilities` for hard boundaries enforced in host imports. Use
`deny_tools` for a hard component-tool denial but only a soft denial of an
agent-internal built-in. `deny_groups` is also soft: agent-core suppresses group
members even when attention grouping is disabled, but a rewritable agent is not
a security boundary. Deny the underlying capability for authoritative control.

`spend_limit_usd` is cumulative across the user's conversations. The global
session ceiling remains a separate limit. `see_all_sessions` is opt-in even for
admins.

Set `discord_id` on a web user to bind that Discord identity to the same owner.
Unbound Discord conversations receive synthetic `discord:*` owners and the
`auth.discord_role` policy.

## Verify

Run the native and guest suites after changing this boundary:

```sh
cargo test -p thetis --lib
cargo test --manifest-path agents/agent-core/Cargo.toml
cargo test --manifest-path gateways/gateway-web/Cargo.toml
```

In a scratch users-mode instance, log in as two users and verify each sidebar
and transcript recall excludes the other's conversation; opening or subscribing
to the other's id must fail. Verify `/admin` is forbidden to a non-admin and
`/ws` returns 401 without a cookie.

## Known gaps

The shared `/workspace` is not partitioned by account. Password reset, OAuth,
TOTP, and conversation sharing are not implemented. Login lockout counters are
process-local and reset when the gateway restarts. Per-name built-in and group
denials remain soft; capability denials are the hard boundary.
