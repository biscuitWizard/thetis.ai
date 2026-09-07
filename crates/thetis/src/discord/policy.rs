//! Routing and authorization decisions, as pure functions.
//!
//! None of this touches the network or the database, so the rules that decide
//! who may talk to the bot and which messages it answers are testable directly.
//! That matters more here than elsewhere: these are the security decisions.

use crate::config::DiscordSettings;

use super::api::Incoming;

/// Why a message is not being answered, or that it is.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Run a turn for it.
    Answer,
    /// Say nothing at all. Used where a reply would be noise or a leak.
    Ignore(&'static str),
    /// Tell the user they are not allowed.
    Unauthorized,
}

/// Whether the bot should respond to this message.
///
/// The order matters. Authorization is checked before the mention rules, so an
/// unauthorized user cannot learn anything about the bot's configuration by
/// probing which channels it answers in.
pub fn decide(cfg: &DiscordSettings, bot_id: &str, paired: &[String], msg: &Incoming) -> Decision {
    // Never answer another bot, or ourselves. Two bots that answer each other
    // will do so until someone intervenes.
    if msg.author_is_bot {
        return Decision::Ignore("the author is a bot");
    }
    if msg.author_id == bot_id {
        return Decision::Ignore("the message is our own");
    }

    if !cfg.authorized(&msg.author_id, paired) {
        return Decision::Unauthorized;
    }

    // A DM is unambiguous: there is nobody else it could be meant for.
    if msg.is_dm() {
        if msg.content.trim().is_empty() {
            return Decision::Ignore("the message is empty");
        }
        return Decision::Answer;
    }

    let mentions_bot = msg.mentions.iter().any(|m| m == bot_id);
    let free_channel = cfg
        .free_response_channels
        .iter()
        .any(|c| c == &msg.channel_id);

    if mentions_bot {
        return Decision::Answer;
    }

    // A message aimed at other people is not ours to answer, even in a channel
    // where a mention is not otherwise required.
    if cfg.ignore_no_mention && !msg.mentions.is_empty() {
        return Decision::Ignore("the message mentions other people, not the bot");
    }

    if free_channel || !cfg.require_mention {
        if msg.content.trim().is_empty() {
            return Decision::Ignore("the message is empty");
        }
        return Decision::Answer;
    }

    Decision::Ignore("no mention in a channel that requires one")
}

/// The key identifying which conversation a message belongs to.
///
/// Shaped after Hermes' `agent:main:{platform}:{chat_type}:{chat_id}` so the
/// routing is recognisable, with the user appended when a shared channel is
/// partitioned per person. A thread is its own channel id on Discord, so
/// threads fall out as separate conversations without special handling.
pub fn session_key(cfg: &DiscordSettings, msg: &Incoming) -> String {
    session_key_for(
        cfg,
        msg.is_dm(),
        msg.in_thread,
        &msg.channel_id,
        &msg.author_id,
    )
}

/// The same key, from parts rather than from a message.
///
/// A slash command arrives as an interaction, not a message, and must land in
/// the same conversation as the surrounding chat — otherwise `/new` would reset
/// a session nobody is talking in. Both paths therefore go through this.
pub fn session_key_for(
    cfg: &DiscordSettings,
    is_dm: bool,
    in_thread: bool,
    channel_id: &str,
    user_id: &str,
) -> String {
    let chat_type = if is_dm {
        "private"
    } else if in_thread {
        "thread"
    } else {
        "channel"
    };

    let mut key = format!("discord:{chat_type}:{channel_id}");

    // A DM is already one person, so partitioning it again would be noise.
    if cfg.group_sessions_per_user && !is_dm {
        key.push(':');
        key.push_str(user_id);
    }
    key
}

/// Discord thread channel types: announcement, public, private.
pub fn is_thread_type(channel_type: Option<u64>) -> bool {
    matches!(channel_type, Some(10) | Some(11) | Some(12))
}

/// Whether a mapped session may carry on being used.
///
/// `None` is a session that no longer exists; `Some(archived)` is one that
/// does. Archiving is a deliberate "I am done with this" — it stops the worker
/// and releases the checkout — so a chat surface must not reopen it. Kept as a
/// pure function because the interesting case, the archived one, is otherwise
/// only reachable by driving a live database.
pub fn may_reuse_session(existing: Option<bool>) -> bool {
    matches!(existing, Some(false))
}

/// Strips the bot's own mention from the text.
///
/// The model does not need to see `<@1234>` at the front of every message, and
/// leaving it in encourages it to echo the raw form back.
pub fn strip_mention(content: &str, bot_id: &str) -> String {
    content
        .replace(&format!("<@{bot_id}>"), "")
        .replace(&format!("<@!{bot_id}>"), "")
        .trim()
        .to_string()
}

/// Normalises a pasted Discord id.
///
/// People paste ids as `<@123>`, `<@!123>` or `user:123` from the Discord UI and
/// from other tools. Accepting those spellings avoids an allowlist that looks
/// correct and silently matches nothing.
pub fn clean_id(entry: &str) -> String {
    let mut entry = entry.trim();
    if entry.starts_with("<@") && entry.ends_with('>') {
        entry = entry
            .trim_start_matches("<@")
            .trim_start_matches('!')
            .trim_end_matches('>');
    }
    if let Some(rest) = entry.strip_prefix("user:") {
        entry = rest;
    }
    entry.trim().to_string()
}

/// A display name fit to put in a transcript.
///
/// Always sanitised, because it is user-controlled and it ends up rendered
/// beside the message. Control characters are stripped so a name cannot span
/// lines, and the length is bounded so it cannot crowd out the message.
///
/// The bracket and colon stripping is kept from when identity travelled inside
/// the message text as `[Name] said this`: there, a display name containing a
/// bracket could forge a second speaker. Authorship is a structured field now,
/// so that specific forgery is gone — but the same name is still interpolated
/// into prose for the model to read (`author_prefix` in the agent), and a name
/// that looks like punctuation is a nuisance wherever it is shown.
pub fn display_name(raw: &str) -> String {
    let name: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
    name.replace(['[', ']', ':'], "").trim().to_string()
}

/// Who a Discord message is from, as the contract's `author` record.
///
/// `id` is what authority is resolved from, so it is the *account* this
/// snowflake is bound to, or a synthetic `discord:` owner when it is bound to
/// nothing — never the snowflake alone, which names no principal.
///
/// Attribution is now unconditional. It used to be added only in a channel and
/// omitted in a DM, on the grounds that a DM has one speaker and a prefix would
/// be clutter. That reasoning applied to text; a structured field is not
/// clutter, and a transcript where some messages carry an author and others do
/// not is worse to reason about than one where they all do — particularly for a
/// conversation that a DM and a channel can both reach.
pub fn author_of(msg: &Incoming, account_id: &str) -> crate::bindings::types::Author {
    crate::bindings::types::Author {
        id: account_id.to_string(),
        display: display_name(&msg.author_name),
        surface: "discord".into(),
    }
}

/// Why a fork was refused, as something sayable to the person who asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkRefusal {
    /// The operator has not turned `/fork` on.
    Disabled,
    /// This Discord identity is not bound to a Thetis account, so there is no
    /// authority to fork *under*. Discord confers none of its own.
    Unbound,
}

/// Whether this Discord identity may start a fork, given the account it
/// resolves to.
///
/// The argument is the **resolved account** — the output of
/// `AuthSettings::owner_for_discord` — and that is load-bearing. The tempting
/// check is against the session key or the channel, and it fails open: with
/// `group_sessions_per_user = false` a channel's key carries no user id at all,
/// so a key-based check would authorise every person in the channel to fork
/// under whoever happened to bind an account there.
///
/// A synthetic `discord:` owner is refused. Those are minted per channel for
/// unbound snowflakes and carry the read-only Discord policy; forking under one
/// would produce a conversation with the same ceiling it started with, which is
/// pointless, and it would let an unbound stranger create conversations named
/// after a channel. Binding is deliberate: an administrator puts `discord_id`
/// on a `[[users]]` entry in the config file. That act is the entire chain of
/// custody between a snowflake and a set of permissions, so nothing may
/// shortcut it.
pub fn may_fork(cfg: &DiscordSettings, account: &str) -> Result<(), ForkRefusal> {
    if !cfg.allow_fork {
        return Err(ForkRefusal::Disabled);
    }
    if is_synthetic_owner(account) {
        return Err(ForkRefusal::Unbound);
    }
    Ok(())
}

/// Whether this speaker may go on prompting an existing fork.
///
/// A fork keeps its ceiling for its whole life, so speaking into one is asking
/// work to be done with somebody else's permissions. Only the account that
/// authorised it may, and the comparison is again on resolved accounts rather
/// than snowflakes or channels.
///
/// Note what this is *not*: it is not the security boundary. Even if this
/// returned true for the wrong person, `effective(turn) = policy(speaker) ∩
/// ceiling(session)` means their turn would run with their own permissions
/// narrowed by the fork's — they cannot borrow authority by typing in someone
/// else's conversation. This check exists so the fork is not a shared workspace
/// people can nudge sideways, and so a refusal is legible rather than a turn
/// that mysteriously cannot do what the fork was made for.
pub fn may_prompt_fork(fork_owner: &str, speaker: &str) -> bool {
    !is_synthetic_owner(speaker) && !speaker.is_empty() && fork_owner == speaker
}

/// Whether an owner id is one of the synthetic per-channel ones minted for a
/// Discord identity bound to no account. Matched by prefix, the same way
/// `Config::policy_for` matches it when handing out the Discord policy.
fn is_synthetic_owner(owner: &str) -> bool {
    owner.starts_with("discord:")
}

/// The KV key mapping a Discord conversation key to its fork.
///
/// Deliberately a different row from `discord.session.{key}`. Sharing one would
/// mean the channel's ordinary chat — which anyone authorised may send, and
/// which is meant to be read-only — got redirected into the write-enabled
/// conversation. The two live side by side: ambient chat in the channel, the
/// fork in its thread.
pub fn fork_key(key: &str) -> String {
    format!("discord.fork.{key}")
}

/// The KV key mapping a Discord conversation key to its ordinary session.
pub fn session_map_key(key: &str) -> String {
    format!("discord.session.{key}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    pub(crate) fn settings() -> DiscordSettings {
        DiscordSettings {
            enabled: true,
            bot_token: None,
            mode: "chat".into(),
            allowed_users: vec!["alice".into()],
            admin_users: Vec::new(),
            allow_all_users: false,
            require_mention: true,
            free_response_channels: Vec::new(),
            ignore_no_mention: true,
            group_sessions_per_user: true,
            stream_edit_interval: Duration::from_millis(1200),
            pairing_code_ttl: Duration::from_secs(900),
            allow_fork: false,
        }
    }

    fn msg(author: &str, guild: Option<&str>, content: &str) -> Incoming {
        Incoming {
            message_id: "m".into(),
            channel_id: "chan".into(),
            guild_id: guild.map(String::from),
            author_id: author.into(),
            author_name: "Alice".into(),
            author_is_bot: false,
            content: content.into(),
            mentions: Vec::new(),
            in_thread: false,
        }
    }

    #[test]
    fn a_dm_from_an_allowed_user_is_answered_without_a_mention() {
        let m = msg("alice", None, "hello");
        assert_eq!(decide(&settings(), "bot", &[], &m), Decision::Answer);
    }

    #[test]
    fn an_unknown_user_is_refused() {
        let m = msg("mallory", None, "hello");
        assert_eq!(decide(&settings(), "bot", &[], &m), Decision::Unauthorized);
    }

    #[test]
    fn a_paired_user_is_allowed() {
        let m = msg("bob", None, "hello");
        let paired = vec!["bob".to_string()];
        assert_eq!(decide(&settings(), "bot", &paired, &m), Decision::Answer);
    }

    #[test]
    fn another_bot_is_never_answered_even_if_it_would_be_authorized() {
        let mut cfg = settings();
        cfg.allow_all_users = true;
        let mut m = msg("other", None, "hello");
        m.author_is_bot = true;
        assert!(matches!(decide(&cfg, "bot", &[], &m), Decision::Ignore(_)));
    }

    #[test]
    fn our_own_message_is_ignored() {
        let mut cfg = settings();
        cfg.allow_all_users = true;
        let m = msg("bot", Some("g"), "hello");
        assert!(matches!(decide(&cfg, "bot", &[], &m), Decision::Ignore(_)));
    }

    #[test]
    fn a_channel_message_without_a_mention_is_ignored() {
        let m = msg("alice", Some("g"), "hello");
        assert!(matches!(
            decide(&settings(), "bot", &[], &m),
            Decision::Ignore(_)
        ));
    }

    #[test]
    fn a_channel_message_mentioning_the_bot_is_answered() {
        let mut m = msg("alice", Some("g"), "<@bot> hello");
        m.mentions = vec!["bot".into()];
        assert_eq!(decide(&settings(), "bot", &[], &m), Decision::Answer);
    }

    #[test]
    fn a_free_response_channel_needs_no_mention() {
        let mut cfg = settings();
        cfg.free_response_channels = vec!["chan".into()];
        let m = msg("alice", Some("g"), "hello");
        assert_eq!(decide(&cfg, "bot", &[], &m), Decision::Answer);
    }

    #[test]
    fn a_message_aimed_at_someone_else_is_left_alone_in_a_free_channel() {
        // The bot should not butt into a conversation between two other people
        // just because the channel does not require mentions.
        let mut cfg = settings();
        cfg.free_response_channels = vec!["chan".into()];
        let mut m = msg("alice", Some("g"), "<@carol> what do you think?");
        m.mentions = vec!["carol".into()];
        assert!(matches!(decide(&cfg, "bot", &[], &m), Decision::Ignore(_)));
    }

    #[test]
    fn authorization_is_checked_before_the_mention_rules() {
        // Otherwise an outsider could map the bot's configuration by watching
        // which channels produce a refusal and which produce silence.
        let m = msg("mallory", Some("g"), "hello");
        assert_eq!(decide(&settings(), "bot", &[], &m), Decision::Unauthorized);
    }

    #[test]
    fn each_user_gets_their_own_session_in_a_shared_channel() {
        let cfg = settings();
        let a = session_key(&cfg, &msg("alice", Some("g"), "hi"));
        let b = session_key(&cfg, &msg("bob", Some("g"), "hi"));
        assert_ne!(a, b);
        assert!(a.ends_with(":alice"));
    }

    #[test]
    fn a_shared_channel_can_be_one_conversation() {
        let mut cfg = settings();
        cfg.group_sessions_per_user = false;
        let a = session_key(&cfg, &msg("alice", Some("g"), "hi"));
        let b = session_key(&cfg, &msg("bob", Some("g"), "hi"));
        assert_eq!(a, b);
    }

    #[test]
    fn a_dm_is_never_partitioned_by_user() {
        let cfg = settings();
        let key = session_key(&cfg, &msg("alice", None, "hi"));
        assert_eq!(key, "discord:private:chan");
    }

    #[test]
    fn a_slash_command_lands_in_the_same_conversation_as_the_chat() {
        // The whole point of sharing `session_key_for`: if an interaction
        // computed a different key, /new would reset a session nobody is in.
        let cfg = settings();
        let mut m = msg("alice", Some("g"), "hi");
        m.channel_id = "chan".into();
        let from_message = session_key(&cfg, &m);
        let from_interaction = session_key_for(&cfg, false, false, "chan", "alice");
        assert_eq!(from_message, from_interaction);
    }

    #[test]
    fn a_slash_command_in_a_dm_matches_the_dm_conversation() {
        let cfg = settings();
        let m = msg("alice", None, "hi");
        assert_eq!(
            session_key(&cfg, &m),
            session_key_for(&cfg, true, false, "chan", "alice")
        );
    }

    #[test]
    fn a_live_conversation_is_reused() {
        assert!(may_reuse_session(Some(false)));
    }

    #[test]
    fn an_archived_conversation_is_never_continued() {
        // Archiving stops the worker and releases the checkout. Carrying on in
        // it from Discord would undo an explicit decision made elsewhere.
        assert!(!may_reuse_session(Some(true)));
    }

    #[test]
    fn a_vanished_conversation_is_not_reused() {
        assert!(!may_reuse_session(None));
    }

    #[test]
    fn discord_thread_channel_types_are_recognised() {
        for kind in [10, 11, 12] {
            assert!(is_thread_type(Some(kind)));
        }
        // 0 is a guild text channel and 1 a DM; neither is a thread, and an
        // absent type must not be guessed as one.
        for kind in [0, 1, 2, 5] {
            assert!(!is_thread_type(Some(kind)));
        }
        assert!(!is_thread_type(None));
    }

    #[test]
    fn a_thread_is_its_own_conversation() {
        let cfg = settings();
        let mut m = msg("alice", Some("g"), "hi");
        m.in_thread = true;
        m.channel_id = "thread-1".into();
        assert!(session_key(&cfg, &m).starts_with("discord:thread:thread-1"));
    }

    #[test]
    fn the_bots_own_mention_is_stripped() {
        assert_eq!(strip_mention("<@bot> hello there", "bot"), "hello there");
        assert_eq!(strip_mention("<@!bot> hi", "bot"), "hi");
    }

    #[test]
    fn pasted_id_spellings_are_accepted() {
        assert_eq!(clean_id("<@123>"), "123");
        assert_eq!(clean_id("<@!123>"), "123");
        assert_eq!(clean_id("user:123"), "123");
        assert_eq!(clean_id("  123 "), "123");
    }

    #[test]
    fn an_author_names_the_account_not_the_snowflake() {
        // What the id must be is the whole point: authority is resolved from
        // it, and a Discord snowflake names no principal. `author_of` is handed
        // the already-resolved account precisely so this cannot be got wrong
        // here — the test pins that it is used verbatim.
        let m = msg("alice", Some("g"), "hi");
        let a = author_of(&m, "account-alice");
        assert_eq!(a.id, "account-alice");
        assert_eq!(a.display, "Alice");
        assert_eq!(a.surface, "discord");
    }

    #[test]
    fn a_dm_is_attributed_too() {
        // It deliberately was not, when identity travelled in the message text
        // and a prefix in a one-speaker conversation was noise. A structured
        // field costs nothing to read and a transcript where attribution comes
        // and goes is harder to reason about than one where it is always there
        // — and the same conversation can be reached from a DM and a channel.
        let m = msg("alice", None, "hi");
        assert_eq!(author_of(&m, "account-alice").display, "Alice");
    }

    #[test]
    fn a_display_name_cannot_forge_an_attribution_line() {
        // This mattered absolutely when identity was a `[Name] ` prefix on the
        // text: a name containing a bracket or a newline could fake a second
        // speaker. Authorship is a field now, so that forgery is structurally
        // gone — but the name is still interpolated into prose for the model
        // and rendered in the UI, so it stays sanitised. Kept as a test rather
        // than dropped, because the reason it is safe changed and the property
        // did not.
        let mut m = msg("alice", Some("g"), "hi");
        m.author_name = "Bad\n[System]".into();
        let a = author_of(&m, "account-alice");
        assert!(
            !a.display.contains('\n'),
            "control characters must be stripped"
        );
        assert_eq!(a.display, "BadSystem");
    }

    #[test]
    fn a_display_name_is_bounded() {
        // Unbounded, a name would crowd the message it is meant to label.
        let mut m = msg("alice", Some("g"), "hi");
        m.author_name = "x".repeat(500);
        assert_eq!(author_of(&m, "a").display.chars().count(), 64);
    }

    #[test]
    fn only_named_admins_may_pair_once_the_list_is_set() {
        let mut cfg = settings();
        assert!(cfg.is_admin("alice"), "the allowlist is the fallback");
        cfg.admin_users = vec!["carol".into()];
        assert!(cfg.is_admin("carol"));
        assert!(!cfg.is_admin("alice"));
    }

    #[test]
    fn a_paired_user_does_not_become_an_admin() {
        // Pairing must not be self-propagating: someone let in by a code should
        // not be able to let others in.
        let cfg = settings();
        assert!(cfg.authorized("bob", &["bob".to_string()]));
        assert!(!cfg.is_admin("bob"));
    }

    #[test]
    fn forking_is_off_until_an_operator_turns_it_on() {
        let cfg = settings();
        assert_eq!(may_fork(&cfg, "alice"), Err(ForkRefusal::Disabled));
    }

    #[test]
    fn only_a_bound_account_may_fork() {
        let mut cfg = settings();
        cfg.allow_fork = true;
        assert_eq!(may_fork(&cfg, "alice"), Ok(()));
        // A synthetic per-channel owner is what an unbound snowflake resolves
        // to. It holds the read-only Discord policy and names no principal, so
        // there is no authority to fork under.
        assert_eq!(
            may_fork(&cfg, "discord:channel:123"),
            Err(ForkRefusal::Unbound)
        );
        assert_eq!(
            may_fork(&cfg, "discord:private:456:789"),
            Err(ForkRefusal::Unbound)
        );
    }

    #[test]
    fn a_fork_is_promptable_only_by_the_account_that_authorised_it() {
        assert!(may_prompt_fork("alice", "alice"));
        assert!(!may_prompt_fork("alice", "bob"));
        // Not even by an unbound stranger who happens to be in the thread, and
        // not by the empty speaker an older gateway would send.
        assert!(!may_prompt_fork("alice", "discord:channel:123"));
        assert!(!may_prompt_fork("alice", ""));
        // And a synthetic owner cannot be prompted even by itself, which is the
        // case that would otherwise let two unbound people in one channel share
        // a conversation as though they were one account.
        assert!(!may_prompt_fork("discord:channel:1", "discord:channel:1"));
    }

    #[test]
    fn a_fork_never_shares_the_channels_own_conversation_row() {
        // Sharing the row would redirect ambient channel chat — which anyone
        // authorised may send, and which is meant to be read-only — into the
        // write-enabled conversation.
        let key = "discord:channel:123";
        assert_ne!(fork_key(key), session_map_key(key));
        assert!(fork_key(key).starts_with("discord.fork."));
        assert!(session_map_key(key).starts_with("discord.session."));
    }

    #[test]
    fn a_thread_gets_its_own_conversation_key_without_being_asked() {
        // This is why a fork lives in a thread: `session_key_for` already
        // partitions on thread-ness, so the fork's thread cannot collide with
        // the parent channel's key even before the separate KV row.
        let cfg = settings();
        let channel = session_key_for(&cfg, false, false, "chan", "alice");
        let thread = session_key_for(&cfg, false, true, "thread", "alice");
        assert_ne!(channel, thread);
    }
}
