---
name = "The mooR event log and history replay"
brief = "What mooR stores as player history, why every record is age-encrypted with a key the server never has, and how a client replays scrollback."
when_to_use = "Use when working on persistent history or scrollback in mooR, or when history is empty, cannot be decrypted, or must be deleted for a person. Not for live event delivery and sequence numbers (read daemon-and-rpc), not for the session buffers that feed it (read hosts-and-sessions), and not for the FlatBuffer record shape (read wire-schema) or Thetis internals."
universal = false
tags = ["moor", "event log", "history", "scrollback", "encryption", "age", "argon2", "set_pubkey", "get_pubkey", "/v1/event-log", "historyrecall", "presentations", "notify", "player_event_log_stats", "purge_player_event_log", "enable-eventlog", "privacy", "fjall"]
version = 2
---
