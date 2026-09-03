//! A Discord fork runs under one account's own permissions, and only that
//! account can talk to it.
//!
//! The pure rules live in `discord::policy` and are unit-tested there. What this
//! file adds is the part those cannot reach: the same rules driven against a
//! real store, so the claims that involve *persistence* are checked against
//! actual rows rather than a stubbed answer. Three of them matter enough to
//! justify a whole test binary:
//!
//! 1. A fork's ceiling is the authorising account's own policy, stored once.
//! 2. The fork mapping is a different row from the channel's ordinary one, so
//!    ambient read-only chat is never redirected into the write-enabled
//!    conversation.
//! 3. Routing sends the authorising account to the fork and everyone else to
//!    the read-only conversation — including in the shared-channel
//!    configuration, which is the case a key-based check gets wrong.
//!
//!     cargo test -p thetis --test discord_fork

use thetis::discord::policy::{
    fork_key, may_prompt_fork, may_reuse_session, session_map_key, ForkRefusal,
};
use thetis::policy::{Cap, EffectivePolicy};
use thetis::store::Store;

const SCOPE: &str = "global";

/// The read-only ceiling every ordinary Discord conversation is stamped with.
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

/// A write-enabled account's own policy — what a fork's ceiling is taken from.
fn writer_policy() -> EffectivePolicy {
    EffectivePolicy {
        admin: true,
        read_only: false,
        denied: Default::default(),
        models: vec![],
        default_model: String::new(),
        modes: vec![],
        default_mode: "agent".into(),
        deny_tools: vec![],
        deny_groups: vec![],
        spend_limit_usd: 0.0,
        max_children: 4,
        see_all_sessions: false,
        models_restricted: false,
    }
}

/// The ordinary read-only conversation for a Discord conversation key.
///
/// Mirrors the real `session_for`, including the ceiling re-assertion it does
/// on a reused conversation: a missing ceiling row is repaired to the Discord
/// default rather than trusted, because absence means "nothing narrows the
/// speaker".
fn session_for(store: &Store, key: &str) -> String {
    if let Some(existing) = store.kv_get(SCOPE, &session_map_key(key)).unwrap() {
        let found = store.get_session(&existing).unwrap();
        if may_reuse_session(found.as_ref().map(|m| m.archived)) {
            if store.ceiling_of(&existing).unwrap().is_none() {
                store.set_ceiling(&existing, &discord_ceiling()).unwrap();
            }
            return existing;
        }
    }
    let meta = store
        .create_session(Some(format!("Discord {key}")), "chat", "discord:test")
        .unwrap();
    store.set_ceiling(&meta.id, &discord_ceiling()).unwrap();
    store.kv_put(SCOPE, &session_map_key(key), &meta.id).unwrap();
    meta.id
}

/// What `fork_session_for` does, minus the async grip: create a conversation
/// owned by the account, stamp the account's own policy as its ceiling, and map
/// this conversation key to it through the fork's *own* row.
fn fork_session_for(store: &Store, key: &str, account: &str, ceiling: &EffectivePolicy) -> String {
    let meta = store
        .create_session(
            Some(format!("Fork for {account} (Discord)")),
            &ceiling.default_mode,
            account,
        )
        .unwrap();
    store.set_ceiling(&meta.id, ceiling).unwrap();
    store.kv_put(SCOPE, &fork_key(key), &meta.id).unwrap();
    meta.id
}

/// What `fork_for` does: the mapped fork and the account that authorised it, if
/// there is a live one. Empty is how the KV table spells absent.
///
/// `Err` is how the real one reports a fork whose ceiling row has gone missing.
/// Unlike an ordinary Discord conversation that case cannot be repaired — the
/// correct ceiling was a snapshot of an account's policy at a moment that has
/// passed, and re-deriving it now would be the widening C4.4 forbids — so it is
/// refused instead.
fn fork_for(store: &Store, key: &str) -> Result<Option<(String, String)>, String> {
    let Some(id) = store
        .kv_get(SCOPE, &fork_key(key))
        .unwrap()
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let found = store.get_session(&id).unwrap();
    if !may_reuse_session(found.as_ref().map(|m| m.archived)) {
        return Ok(None);
    }
    if store.ceiling_of(&id).unwrap().is_none() {
        return Err(format!("the fork {id} has no ceiling"));
    }
    let owner = store.owner_of_root(&id).unwrap().unwrap_or_default();
    Ok(Some((id, owner)))
}

/// What `route_for` does: the fork when this speaker authorised it, otherwise
/// the read-only conversation.
fn route_for(store: &Store, key: &str, account: &str) -> Result<String, String> {
    if let Some((fork, owner)) = fork_for(store, key)? {
        if may_prompt_fork(&owner, account) {
            return Ok(fork);
        }
    }
    Ok(session_for(store, key))
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();
    (dir, store)
}

#[test]
fn a_fork_carries_the_authorising_accounts_own_permissions() {
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());

    // The two conversations are distinct, and they hold different authority.
    assert_ne!(read_only, fork);
    let parent_ceiling = store.ceiling_of(&read_only).unwrap().expect("a ceiling");
    let fork_ceiling = store.ceiling_of(&fork).unwrap().expect("a ceiling");
    assert!(parent_ceiling.read_only, "ambient chat stays read-only");
    assert!(!fork_ceiling.read_only, "the fork is what can write");
    assert!(
        parent_ceiling.denies(Cap::Devkit),
        "read-only must imply no devkit, whatever the mode says"
    );
    assert!(
        !fork_ceiling.denies(Cap::Devkit),
        "the fork can do what its account can do"
    );
}

#[test]
fn forking_does_not_change_what_the_channel_could_already_do() {
    // The user's requirement, stated as a test: asking for a fork must not
    // influence the permissions the Discord conversation already had.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);
    let before = store.ceiling_of(&read_only).unwrap().expect("a ceiling");

    fork_session_for(&store, key, "writer", &writer_policy());

    let after = store.ceiling_of(&read_only).unwrap().expect("a ceiling");
    assert_eq!(
        serde_json::to_string(&before).unwrap(),
        serde_json::to_string(&after).unwrap(),
        "the parent conversation's ceiling moved"
    );
    // And the channel's own mapping still points where it did.
    assert_eq!(session_for(&store, key), read_only);
}

#[test]
fn ambient_chat_is_never_redirected_into_the_fork() {
    // Sharing one KV row between the channel's conversation and its fork would
    // hand the write-enabled conversation to anyone authorised to chat.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());

    assert_eq!(
        store.kv_get(SCOPE, &session_map_key(key)).unwrap(),
        Some(read_only.clone())
    );
    assert_eq!(store.kv_get(SCOPE, &fork_key(key)).unwrap(), Some(fork));
    assert_eq!(
        session_for(&store, key),
        read_only,
        "the channel's own row was overwritten"
    );
}

#[test]
fn only_the_authorising_account_is_routed_to_the_fork() {
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());

    assert_eq!(route_for(&store, key, "writer").unwrap(), fork);
    assert_eq!(route_for(&store, key, "reader").unwrap(), read_only);
    // An unbound Discord identity resolves to a synthetic owner, which is
    // refused even though it is the only "account" it has.
    assert_eq!(route_for(&store, key, "discord:channel:c1").unwrap(), read_only);
}

#[test]
fn a_shared_channel_key_does_not_leak_the_fork_to_the_room() {
    // With `group_sessions_per_user = false` the conversation key carries no
    // user id, so everyone in the channel shares it. This is exactly the
    // configuration in which a check keyed on the channel or the session key
    // would authorise the whole room; the routing decision is on the resolved
    // account, so it does not.
    let (_dir, store) = store();
    let key = "discord:channel:c1"; // no `:user` suffix

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());

    assert_eq!(route_for(&store, key, "writer").unwrap(), fork);
    for other in ["reader", "someone-else", "discord:channel:c1"] {
        assert_eq!(
            route_for(&store, key, other).unwrap(),
            read_only,
            "{other} reached a fork they did not authorise"
        );
    }
}

#[test]
fn a_fork_stays_the_same_conversation_across_messages() {
    // The point of binding a fork rather than spawning a sub-agent: it is still
    // there next time you speak, so it can be followed up.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let fork = fork_session_for(&store, key, "writer", &writer_policy());
    for _ in 0..3 {
        assert_eq!(route_for(&store, key, "writer").unwrap(), fork);
    }
}

#[test]
fn unbinding_a_fork_sends_the_account_back_to_the_read_only_conversation() {
    // `/fork off`. The conversation itself is deliberately left alive — ending
    // work is a decision made where the work can be seen.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());
    assert_eq!(route_for(&store, key, "writer").unwrap(), fork);

    // The KV table has no delete: clearing writes empty.
    store.kv_put(SCOPE, &fork_key(key), "").unwrap();

    assert_eq!(route_for(&store, key, "writer").unwrap(), read_only);
    assert!(fork_for(&store, key).unwrap().is_none());
    assert!(
        store.get_session(&fork).unwrap().is_some(),
        "the fork's transcript must survive being unbound"
    );
}

#[test]
fn an_archived_fork_is_not_silently_resurrected() {
    // Same rule as the ordinary conversation: archiving means finished. Unlike
    // ambient chat, though, a fork is not replaced automatically — it was
    // created by an explicit act, so resuming needs another one.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());
    store.archive_session(&fork, true).unwrap();

    assert!(fork_for(&store, key).unwrap().is_none());
    assert_eq!(
        route_for(&store, key, "writer").unwrap(),
        read_only,
        "an archived fork must not keep receiving messages"
    );
}

#[test]
fn a_forks_ceiling_does_not_widen_when_its_account_does() {
    // The ceiling is a snapshot, and the asymmetry is deliberate. Narrowing the
    // account takes effect at once, because every turn intersects the speaker's
    // live policy with this stored ceiling. Widening the account does not reach
    // back into a fork that already exists.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let mut narrow = writer_policy();
    narrow.denied = [Cap::Devkit].into_iter().collect();
    let fork = fork_session_for(&store, key, "writer", &narrow);

    // The account is later granted the devkit. The fork's stored ceiling is
    // untouched, so it still cannot use it.
    let stored = store.ceiling_of(&fork).unwrap().expect("a ceiling");
    assert!(stored.denies(Cap::Devkit));
    let live = writer_policy();
    assert!(!live.denies(Cap::Devkit));
    assert!(
        live.intersect(&stored).denies(Cap::Devkit),
        "the intersection is what a turn runs under, and it must stay narrow"
    );
}

#[test]
fn only_the_authorising_account_may_put_a_fork_away() {
    // A bug caught while writing this: `/fork off` originally unbound whatever
    // was mapped, without asking whose it was. Unbinding is not destructive,
    // but it decides where someone *else's* messages go — and their next
    // message would be answered by a conversation with none of the context or
    // authority they were relying on.
    let (_dir, store) = store();
    let key = "discord:channel:c1"; // shared channel, so two people are here

    let read_only = session_for(&store, key);
    let fork = fork_session_for(&store, key, "writer", &writer_policy());

    // What the command does before unbinding: find the fork, check the owner.
    let (_, owner) = fork_for(&store, key).unwrap().expect("a bound fork");
    assert!(!may_prompt_fork(&owner, "reader"), "reader must be refused");
    assert!(may_prompt_fork(&owner, "writer"));

    // The refusal leaves the binding intact, so the owner still gets their fork.
    assert_eq!(route_for(&store, key, "writer").unwrap(), fork);
    assert_eq!(route_for(&store, key, "reader").unwrap(), read_only);
}

#[test]
fn a_conversation_that_lost_its_ceiling_is_repaired_before_it_is_reused() {
    // H1, as it survives ceilings. The plan asked for the *mode* to be
    // re-asserted on a mapped conversation; since ceilings landed, the mode is
    // not what bounds anything, and re-stamping it would fight a deliberate
    // change. What must not drift is the ceiling — and the moment a Discord
    // conversation is picked up again, after arbitrary time and arbitrary
    // intervening writes, is the one place to notice.
    //
    // A *missing* ceiling is the dangerous state: absence means nothing narrows
    // the speaker, which for a Discord identity bound to an admin account is
    // the dev kit over chat. So absence is repaired, not trusted.
    // The state is built the way it actually arises: a conversation mapped to a
    // channel *before* ceilings existed. Every Discord conversation from before
    // step 3 looks exactly like this, so this is a migration case and not a
    // hypothetical — which is also why the repair has to happen on the reuse
    // path rather than in a one-off startup sweep.
    let (_dir, store) = store();
    let key = "discord:channel:c1";

    let legacy = store
        .create_session(Some("Discord (old)".into()), "chat", "discord:test")
        .unwrap();
    store.kv_put(SCOPE, &session_map_key(key), &legacy.id).unwrap();
    assert!(
        store.ceiling_of(&legacy.id).unwrap().is_none(),
        "the premise: nothing narrows this conversation"
    );

    // Picking it up again restores the ceiling, and to the read-only default
    // rather than to whatever the speaker happens to hold.
    let again = session_for(&store, key);
    assert_eq!(again, legacy.id, "the transcript is kept, not abandoned");
    let restored = store.ceiling_of(&legacy.id).unwrap().expect("restamped");
    assert!(restored.read_only, "the repair must be the read-only default");
    assert!(restored.denies(Cap::Delegation));
}

#[test]
fn a_fork_that_lost_its_ceiling_is_refused_rather_than_rebuilt() {
    // The same drift, but a fork cannot be repaired the way an ordinary Discord
    // conversation can. Its ceiling was a snapshot of an account's policy at the
    // moment it was authorised; re-deriving it now would read the account's
    // *current* policy, which is exactly the widening C4.4 forbids — the whole
    // reason a fork's ceiling is written once and never recomputed.
    //
    // There is no safe value to write, so the fork is refused and the account is
    // told to make a new one. Refusing costs a conversation; guessing costs the
    // guarantee.
    let (_dir, store) = store();
    let key = "discord:channel:c1:u1";

    let read_only = session_for(&store, key);

    // A fork mapping pointing at a conversation with no ceiling row. Written
    // directly, because the real `fork_session_for` cannot produce this state —
    // it archives the conversation if the stamp fails — so the only way to
    // reach it is corruption or a hand-edited row. That is precisely why the
    // reader must not assume it away.
    let unbounded = store
        .create_session(Some("Fork for writer (Discord)".into()), "agent", "writer")
        .unwrap();
    store.kv_put(SCOPE, &fork_key(key), &unbounded.id).unwrap();

    let refused = fork_for(&store, key);
    assert!(refused.is_err(), "an unbounded fork must not be handed out");
    assert!(
        refused.unwrap_err().contains("no ceiling"),
        "the refusal should say what is wrong"
    );

    // And routing refuses too, rather than quietly falling back to the
    // read-only conversation: silently downgrading someone mid-conversation
    // would answer with the wrong authority *and* the wrong transcript.
    assert!(route_for(&store, key, "writer").is_err());
    assert_ne!(
        route_for(&store, key, "writer").ok(),
        Some(read_only),
        "a broken fork must not be papered over with the read-only conversation"
    );
}

#[test]
fn a_refusal_says_which_of_the_two_things_is_wrong() {
    // Not a behaviour test so much as a promise about legibility: "not enabled"
    // and "your account is not linked" need different answers, because they
    // need different people to act.
    assert_ne!(ForkRefusal::Disabled, ForkRefusal::Unbound);
}
