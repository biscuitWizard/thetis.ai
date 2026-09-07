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
pub fn decide(
    cfg: &DiscordSettings,
    bot_id: &str,
    paired: &[String],
    msg: &Incoming,
) -> Decision {
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

/// How a message is attributed to its author for the model.
///
/// The contract's `user-msg` has no author field, so identity has to travel in
/// the text. It is only added where more than one person can be present: a DM
/// is already unambiguous, and prefixing every line there would be clutter the
/// model might start imitating.
///
/// The name is sanitised because it is user-controlled: a display name
/// containing a newline could otherwise forge an attribution line.
pub fn attribute(msg: &Incoming, text: &str, per_user_sessions: bool) -> String {
    if msg.is_dm() || per_user_sessions {
        return text.to_string();
    }
    let name: String = msg
        .author_name
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let name = name.replace(['[', ']', ':'], "");
    format!("[{name}] {text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn settings() -> DiscordSettings {
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
        assert!(matches!(
            decide(&cfg, "bot", &[], &m),
            Decision::Ignore(_)
        ));
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
        let from_interaction =
            session_key_for(&cfg, false, false, "chan", "alice");
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
    fn a_shared_conversation_attributes_each_speaker() {
        let m = msg("alice", Some("g"), "hi");
        assert_eq!(attribute(&m, "hi", false), "[Alice] hi");
    }

    #[test]
    fn a_dm_is_not_attributed() {
        let m = msg("alice", None, "hi");
        assert_eq!(attribute(&m, "hi", false), "hi");
    }

    #[test]
    fn a_display_name_cannot_forge_an_attribution_line() {
        // A name carrying a newline and brackets could otherwise fake a second
        // speaker in the transcript.
        let mut m = msg("alice", Some("g"), "hi");
        m.author_name = "Bad\n[System]".into();
        let text = attribute(&m, "hi", false);
        assert!(!text.contains('\n'), "control characters must be stripped");
        assert_eq!(text, "[BadSystem] hi");
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
}
