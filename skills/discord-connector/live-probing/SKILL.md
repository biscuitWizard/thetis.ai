---
name = "Probing Discord live"
brief = "Drive the real Discord API with the bot's own token to verify sends, edits, typing and components."
when_to_use = "Use when a Discord behaviour needs checking against the real API rather than argued from code — edit cadence, typing indicators, component posting, permissions — or when tempted to write 'unverified without a token'."
tags = ["discord", "verification", "credentials"]
---

# Probing Discord live

The token is on this box. Claiming a Discord behaviour "cannot be verified
without a real token" is wrong, and it is how a wrong diagnosis survives.

## Getting the token

`thetis.toml` has `bot_token = ""`; the real one is in the local overlay, whose
path is in the running process's environment:

```
cat /proc/$(pgrep -f thetis | head -1)/environ \
  | tr '\0' '\n' | grep THETIS_LOCAL_CONFIG      # /opt/thetis/thetis.local.toml
```

That file is `bitmuse`-owned and readable; `/etc/thetis.env` is root-only, which
is a dead end. Read it with a regex on `^bot_token`, and never echo the value.

## The one trap that looks like an auth failure

**Discord's Cloudflare rejects the default `urllib`/`python-requests`
User-Agent with `403 error code: 1010`** — an opaque body, no JSON, on every
endpoint including `/users/@me/guilds`. It reads exactly like a bad token or a
missing permission. Send a real one:

```python
UA = "DiscordBot (https://github.com/biscuitWizard/thetis, 0.1) probe"
```

`curl` is unaffected, since its own UA is acceptable. So a request that works in
`curl` and 403s in Python is this, not permissions.

Also: `HTTPError.read()` on a 1010 is not JSON, so a bare `json.loads` in the
error path raises `JSONDecodeError` and hides the status. Return the raw bytes.

## What to check, and the statuses that mean success

| Call | Expect |
|---|---|
| `GET /users/@me` | 200, `username` is the bot |
| `GET /users/@me/guilds` | 200, the guild list |
| `POST /channels/{ch}/typing` | **204** |
| `POST /channels/{ch}/messages` | 200 with the new id |
| `PATCH /channels/{ch}/messages/{id}` | 200 — this is the streaming edit |
| `DELETE /channels/{ch}/messages/{id}` | 204 — clean up the probe |

Send → edit → edit → delete round-trips the whole streaming path in about five
seconds, which settles whether a stalled reply is Discord's fault or ours.

Probe in `#moor-dev` (`1259119150756925440`) and delete what you post. Always
send `allowed_mentions: {"parse": ["users"]}`, as the connector does, so a probe
cannot ping a role.

## Timer behaviour is testable without Discord at all

The typing indicator vanishing during busy turns was a `select!` bug, not an API
one, and a 40-line standalone tokio binary proved it: a `sleep` built inside
`select!` is recreated every iteration, so a stream of events starves it
forever. Feed events every 2s for 20s and count firings — 0 for the `sleep`
shape, 2 for a `tokio::time::interval` owned outside the loop.

Build such a probe under `/workspace/`, not `/tmp` (the terminal refuses to
leave the checkout) and not in the checkout itself. Give it its own
`[workspace]` key in `Cargo.toml` or cargo tries to join the parent workspace.
