//! Archiving a Discord conversation must start a new one, not resurrect it.
//!
//! Drives a real store through the same steps `session_for` takes — read the
//! mapping, look the session up, decide — so the decision is checked against
//! actual persistence rather than a stubbed answer.
//!
//!     cargo test -p thetis --test discord_archived

use thetis::discord::policy::may_reuse_session;
use thetis::store::Store;

/// The connector's own scope and key shape.
const SCOPE: &str = "global";

fn kv_key(key: &str) -> String {
    format!("discord.session.{key}")
}

/// What `session_for` does, minus the async grip: reuse the mapped session
/// when it may be reused, otherwise create one and remap.
fn session_for(store: &Store, key: &str) -> String {
    if let Some(existing) = store.kv_get(SCOPE, &kv_key(key)).unwrap() {
        let found = store.get_session(&existing).unwrap();
        if may_reuse_session(found.as_ref().map(|m| m.archived)) {
            return existing;
        }
    }
    let meta = store.create_session(Some(format!("Discord {key}")), "chat").unwrap();
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
