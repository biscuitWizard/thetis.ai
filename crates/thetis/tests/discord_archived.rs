//! Archiving a Discord conversation must start a new one, not resurrect it.
//!
//! Drives a real store through the same steps `session_for` takes — read the
//! mapping, look the session up, decide — so the decision is checked against
//! actual persistence rather than a stubbed answer.
//!
//!     cargo test -p thetis --test discord_archived

use thetis::discord::policy::{may_reuse_session, session_map_key};
use thetis::policy::{Cap, EffectivePolicy};
use thetis::store::Store;

/// The connector's own scope and key shape.
const SCOPE: &str = "global";

/// The connector's own key, not a copy of it: a test that hardcodes the
/// spelling stays green while production moves.
fn kv_key(key: &str) -> String {
    session_map_key(key)
}

/// What the connector stamps as a Discord conversation's ceiling: read-only,
/// and no delegating its way out of that.
fn discord_ceiling() -> EffectivePolicy {
    EffectivePolicy {
        admin: false,
        read_only: true,
        denied: [Cap::Delegation].into_iter().collect(),
        models: vec![],
        default_model: String::new(),
        modes: vec![],
        default_mode: "chat".into(),
        deny_tools: vec![],
        deny_groups: vec![],
        spend_limit_usd: 0.0,
        max_children: 0,
        see_all_sessions: false,
        models_restricted: false,
    }
}

/// What `session_for` does, minus the async grip: reuse the mapped session
/// when it may be reused, otherwise create one, stamp its ceiling and remap.
fn session_for(store: &Store, key: &str) -> String {
    if let Some(existing) = store.kv_get(SCOPE, &kv_key(key)).unwrap() {
        let found = store.get_session(&existing).unwrap();
        if may_reuse_session(found.as_ref().map(|m| m.archived)) {
            return existing;
        }
    }
    let meta = store
        .create_session(Some(format!("Discord {key}")), "chat", "discord:test")
        .unwrap();
    store.set_ceiling(&meta.id, &discord_ceiling()).unwrap();
    store.kv_put(SCOPE, &kv_key(key), &meta.id).unwrap();
    meta.id
}

#[test]
fn an_archived_discord_conversation_is_replaced_rather_than_continued() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();
    let key = "discord:channel:c1:u1";

    // First contact creates a conversation, and it is stable while it lives.
    let first = session_for(&store, key);
    assert_eq!(session_for(&store, key), first, "a live session is reused");

    // Someone archives it in the web UI.
    store.archive_session(&first, true).unwrap();

    let second = session_for(&store, key);
    assert_ne!(second, first, "an archived conversation must not be continued");

    // The new one is live, reused from then on, and the mapping has moved.
    assert!(!store.get_session(&second).unwrap().unwrap().archived);
    assert_eq!(session_for(&store, key), second);
    assert_eq!(store.kv_get(SCOPE, &kv_key(key)).unwrap().unwrap(), second);

    // The archived transcript is untouched: this is a fresh start, not a delete.
    let archived = store.get_session(&first).unwrap().expect("still readable");
    assert!(archived.archived);

    // And archiving the replacement starts a third, so the rule is not
    // one-shot.
    store.archive_session(&second, true).unwrap();
    let third = session_for(&store, key);
    assert_ne!(third, second);
    assert_ne!(third, first);
}

#[test]
fn archiving_one_channel_does_not_disturb_another() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();

    let a = session_for(&store, "discord:channel:c1:u1");
    let b = session_for(&store, "discord:channel:c2:u1");
    assert_ne!(a, b);

    store.archive_session(&a, true).unwrap();

    assert_ne!(session_for(&store, "discord:channel:c1:u1"), a);
    assert_eq!(
        session_for(&store, "discord:channel:c2:u1"),
        b,
        "an untouched channel keeps its conversation"
    );
}

/// The replacement conversation is bounded too.
///
/// This is the failure mode the shared `new_session_for` helper exists to
/// prevent: `/new` and first-contact both create conversations, and a second
/// copy of the creation code is a second place to forget the ceiling. A
/// Discord conversation with no ceiling row resolves to the *speaker's own*
/// policy — so for a user whose account is bound via `discord_id`, forgetting
/// the stamp on the replacement would silently hand their full web authority
/// to the channel. Archiving is the ordinary way to get a replacement, which
/// makes it the ordinary way to hit that.
#[test]
fn every_replacement_conversation_is_ceilinged_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();
    let key = "discord:channel:c1:u1";

    let first = session_for(&store, key);
    assert!(store.ceiling_of(&first).unwrap().is_some());

    // Three generations, each one bounded.
    let mut seen = vec![first];
    for _ in 0..3 {
        let previous = *seen.last().as_ref().unwrap();
        store.archive_session(previous, true).unwrap();
        let next = session_for(&store, key);
        assert_ne!(&next, previous);

        let ceiling = store
            .ceiling_of(&next)
            .unwrap()
            .expect("a Discord conversation without a ceiling is unbounded");
        assert!(ceiling.read_only, "the read-only guarantee is the ceiling");
        assert!(ceiling.denies(Cap::Devkit), "and it holds host-side");
        assert!(ceiling.denies(Cap::Delegation), "no delegating out of it");
        assert!(!ceiling.admin);
        // Reading is untouched: a ceiling narrows, it does not disable.
        assert!(!ceiling.denies(Cap::FilesystemRead));
        seen.push(next);
    }
}

/// H1, retired rather than patched.
///
/// A conversation's mode is mutable after creation from the web UI, and
/// `store.set_mode` validates nothing. While the read-only guarantee rested on
/// `discord.mode`, that was an escalation: a user whose account is bound to a
/// Discord channel could open that conversation in the browser, set its mode to
/// an agent mode, and the tool filter in `agents/agent-core` would let the
/// mutating tools through.
///
/// The ceiling is in a different table, so the mode cannot reach it. Setting
/// the mode to anything at all — including a mode that does not exist — leaves
/// the authority exactly where it was.
#[test]
fn changing_the_mode_does_not_change_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();
    let id = session_for(&store, "discord:channel:c1:u1");

    let before = store.ceiling_of(&id).unwrap().unwrap();
    assert!(before.read_only);

    for mode in ["agent", "build", "", "not-a-real-mode-at-all"] {
        store.set_mode(&id, mode).unwrap();
        let after = store
            .ceiling_of(&id)
            .unwrap()
            .expect("the mode must not be able to remove a ceiling");
        assert!(
            after.read_only && after.denies(Cap::Devkit) && !after.admin,
            "mode {mode:?} moved the ceiling"
        );
    }
}

/// A ceiling belongs to a conversation, not to a channel: the archived one
/// keeps its own, and it is not shared or moved.
#[test]
fn a_ceiling_travels_with_the_conversation_not_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();

    let a = session_for(&store, "discord:channel:c1:u1");
    let b = session_for(&store, "discord:channel:c2:u1");
    store.archive_session(&a, true).unwrap();
    let a2 = session_for(&store, "discord:channel:c1:u1");

    for id in [&a, &b, &a2] {
        assert!(
            store.ceiling_of(id).unwrap().unwrap().read_only,
            "every conversation carries its own ceiling"
        );
    }
    // Including the archived one, which stays readable.
    assert!(store.get_session(&a).unwrap().unwrap().archived);
}

#[test]
fn unarchiving_makes_a_conversation_usable_again() {
    // Nothing here is one-way: if the mapping still points at it, restoring a
    // conversation in the UI brings the channel back to it.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();
    let key = "discord:private:d1";

    let first = session_for(&store, key);
    store.archive_session(&first, false).unwrap();
    assert_eq!(session_for(&store, key), first);
}
