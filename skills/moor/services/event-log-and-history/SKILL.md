---
name = "The mooR event log and history replay"
brief = "What mooR stores as player history, why every record is age-encrypted with a key the server never has, how a client replays scrollback, and what that costs an operator."
when_to_use = "Use when working on persistent history or scrollback in mooR: the event log database, set_pubkey and get_pubkey, the /v1/event-log endpoints, HistoryRecall, presentations, event_log() and notify() logging, player_event_log_stats and purge_player_event_log, or the enable-eventlog feature flag. Use it when history is empty, cannot be decrypted, or must be deleted for a person. Not for live event delivery and sequence numbers (read daemon-and-rpc), not for the session buffers that feed it (read hosts-and-sessions), not for the FlatBuffer record shape (read wire-schema), and not for MOO verb code inside a running world such as the Torchship database, which the torchship skills own, or for Thetis internals."
universal = false
tags = ["moor", "event log", "history", "scrollback", "encryption", "age", "argon2", "pubkey", "presentations", "privacy", "enable_eventlog", "fjall"]
version = 1
---

# The mooR event log and history replay

The event log is mooR's answer to "scroll back and see what you missed". It is a
per-player, append-only record of narrative events, stored encrypted with a key
derived from a password the player chose and the server never receives. It is
disabled by default.

## What is stored

Everything a task hands to `Session::send_event` or `Session::log_event` and
that is addressed to a player who has a public key registered:

- Output from `notify()`, including rich values when the feature is on.
- Output from `event_log()`, which writes to history *without* broadcasting to
  live connections. This is how a world can send a pretty rendering to the
  terminal and a canonical one to the record.
- Tracebacks and error output.
- Presentations. Their identifier stays plaintext so a dismiss can find them;
  the content is encrypted like everything else. Current presentation state is
  kept separately from the event stream, so a reconnecting client can restore the
  panels it had open without replaying the whole log.

Each record holds an event id, a nanosecond timestamp, the player, and one
encrypted blob. The record shape is in
`crates/schema/schema/moor_event_log.fbs`. Event ids are UUID v7, so they sort
chronologically and the log can be queried by id range without a separate index
on time.

## The rule that makes it private

**No public key, no history.** When a session commits, it looks up the history
owner's public key. If there is none, the event is skipped entirely. There is no
plaintext fallback and no configuration that enables one.

That single rule is the whole privacy design:

1. The player picks an event-log password, separate from the MOO login password.
2. The client derives 32 bytes with Argon2id, salted with a fixed string and the
   player's object id, so the same password on any device produces the same key.
3. The client turns those bytes into an age (X25519) identity and sends only the
   **public** key to the server, which stores it against the player.
4. The daemon encrypts each event to that public key before it ever reaches disk.
5. History comes back to the client as encrypted blobs. The client decrypts them.

The daemon has no production decryption path at all: the decrypt function in
`crates/daemon/src/event_log/encryption.rs` is compiled only for tests. Neither
the daemon, nor the web host, nor an administrator with the database files can
read what a player said, unless they have the player's password.

The limits are honest ones, and the design document states them: this protects
against stolen backups, filesystem snooping and casual administrative reading. It
does not protect against a modified daemon binary, because the daemon must
generate the events in the first place, and it does not protect live events in
flight.

## History ownership, not just the player

A session carries an **active player** and a **history owner**. They are usually
the same object. `switch_player()` can move the active player while leaving the
history owner where it was, so an administrator acting as another character does
not write into that character's private log. On commit, an event addressed to the
session's active player is logged under the history owner; an event addressed to
any other player is logged under that player.

Get this wrong and you write one person's words into another person's encrypted
log, where nobody can find them again to remove them.

## Replaying history to a client

Only the web path implements history. The browser asks the web host, the web host
asks the daemon, and the encrypted blobs pass through untouched.

Four recall modes exist (`HistoryRecall` in `moor_rpc.fbs`): since a given event
id, until a given event id, since a number of seconds ago, and none. Each takes a
limit, where zero means no limit. "Since" keeps the oldest events up to the
limit; "until" and "since seconds" keep the most recent, because those are the
ones a client scrolling upwards wants. The response also reports the total
available, whether more exist before the window, and the earliest and latest ids
in the window, so a client can page.

The telnet host implements none of this. It cannot supply a key and never sends a
history request, so a telnet user sees live output only. Events from a telnet
session are still logged, encrypted, if that player has a key.

Do not confuse this with the live-event replay described in `daemon-and-rpc`.
That one recovers events a lossy PUB socket dropped seconds ago, is not
encrypted, is bounded in memory, and disappears on daemon restart. The event log
is durable, encrypted and unbounded. They are different mechanisms with similar
names.

## Managing and deleting history

| Operation | Reached by |
|---|---|
| Read or set the player's public key | The client RPC pair, or `GET`/`PUT` on the web host's event-log pubkey endpoint |
| Delete all of a player's history | The delete client request, or `DELETE` on the web host's event-log history endpoint |
| Report size and time range | `player_event_log_stats()` in MOO, for the player's owner or a wizard |
| Purge, optionally before a timestamp, optionally dropping the key | `purge_player_event_log()` in MOO |

`purge_player_event_log()` returns how many events it deleted and whether the
public key went with them. Dropping the key is what stops new events being
recorded at all, because of the no-key-no-history rule.

## Operational consequences

These are the things an operator is surprised by.

- **A lost event-log password is a lost log.** There is no recovery, no escrow
  and no reset. The password must be written down. Say so in the client's
  onboarding, as the reference web client does.
- **Changing the password orphans the old records.** New events encrypt to the
  new key; old ones still need the old one. Treat a key change as starting a new
  log.
- **You cannot answer "what did they say?"** by inspection. Moderation and
  incident response have to work from live observation or from what a
  participant chooses to reveal.
- **You can still answer "how much, and when?"** The event id, timestamp and
  player are plaintext. That is enough for retention policy and for a deletion
  request, and it is also metadata that leaks; note it in a privacy statement.
- **The events database fsyncs after every event.** The event log's persistence
  thread writes each record and then performs a full synchronous persist. It is
  the most durable thing in the daemon's data directory and the most write-heavy.
  See the durability table in `daemon-and-rpc`. Put it on storage that can take
  the write rate, and budget for it before enabling the feature.
- **The log grows without bound.** Nothing expires records. Retention is a policy
  you implement with `purge_player_event_log()`.
- **Enabling it is a decision, not a default.** With the feature off the daemon
  installs a no-op event log, and nothing is written. The current default is in
  the feature configuration, not in this file.

## Invariants

1. **Every stored event is encrypted.** There is no code path that writes
   plaintext event content, and none may be added.
2. **The server never holds a private key.** It stores public keys only. Do not
   add an endpoint that accepts a private key or a password.
3. **No public key means the event is dropped, silently, at commit.** This is by
   design, and it is the first thing to check when history is empty.
4. **Events are logged at session commit, with the live batch.** A rolled-back
   transaction logs nothing, so history and live output cannot diverge.
5. **The event id in history is the same id the live event carried.** The
   conversion deliberately preserves it, so a client can join the two streams.
6. **The presentation id is plaintext; the presentation content is not.**
7. **Changing `moor_event_log.fbs` rewrites the meaning of records already on
   disk.** Read the evolution rules in `wire-schema` first.

## Where the code lives

| Path | Responsibility |
|---|---|
| `crates/daemon/src/event_log/event_log.rs` | The `EventLogOps` trait, the Fjall store, the persistence thread, queries, stats and purge |
| `crates/daemon/src/event_log/encryption.rs` | age encryption; the decrypt half is test-only |
| `crates/daemon/src/event_log/event_log_conversions.rs` | Domain event to encrypted record, and the presentation action it extracts |
| `crates/daemon/src/rpc/session.rs` | Where history ownership is resolved and the key looked up |
| `crates/daemon/src/rpc/message_handler_history.rs` | The four recall modes and the response window |
| `crates/schema/schema/moor_event_log.fbs` | The stored record shape |
| `crates/web-host/src/host/handlers/event_log.rs` | The browser-facing history, pubkey and delete endpoints |
| `crates/kernel/src/vm/builtins/bf_connection.rs` | `event_log()` |
| `crates/kernel/src/vm/builtins/bf_server.rs` | `player_event_log_stats()`, `purge_player_event_log()` |
| `doc/event-log-encryption-design.md` | The threat model and the client-side derivation, with parameters |
| `book/src/the-system/event-logging.md` | The operator-facing description |

## Failure branches

| Symptom | Cause | Action |
|---|---|---|
| History is always empty for one player | That player has no public key stored | Have the client set an event-log password. Nothing is logged until it does |
| History is always empty for everyone | The event log feature is off, so the daemon installed the no-op log | Enable it in the daemon's feature configuration and restart |
| Client shows history it cannot decrypt | The password changed, or the client derived the key with different parameters | Old records need the old key. Check the derivation salt uses the player's object id |
| A telnet user reports missing scrollback | The telnet host has no history support | Expected. Not a bug to fix in the telnet host without also solving key handling |
| Events appear in the wrong player's history after `switch_player` | The history owner moved with the active player | Preserve history on the switch. There is no way to move records afterwards; they are encrypted to the wrong key |
| The events database grows faster than expected | Every event, including `event_log()`-only records, is stored and fsynced | Apply a retention policy with `purge_player_event_log()` |
| Write latency rises under load with the log on | The per-event synchronous persist | Move the events database to faster storage, or leave the feature off |
| An old events database will not read after an upgrade | The stored record schema changed | See `wire-schema`. The payload is encrypted, so a decode error surfaces only when someone scrolls back |

## Read first / read next

- Read `hosts-and-sessions` for the buffers that feed the log and for the
  history-owner rule at its source.
- Read `daemon-and-rpc` for the live-event replay it is easy to confuse this
  with, and for the durability table.
- Read `wire-schema` before changing the record shape.
- Read `clients-and-web-ui` for the client half: the history and presentation
  endpoints, and what a client must do when it has no key.
- `doc/event-log-encryption-design.md` is accurate about the design and states
  its own limits; it is the right document to hand an operator asking about
  privacy.
