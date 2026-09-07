//! The commands the connector answers, and the schema Discord needs to offer
//! them in the slash-command picker.
//!
//! There are two ways in and one implementation. A registered slash command
//! arrives as an INTERACTION_CREATE; the same words typed as ordinary text
//! arrive as a MESSAGE_CREATE. Discord intercepts anything starting with `/`, so
//! the text path only sees a command when the picker did not match — a stale
//! client, or the first hour after a global registration. Both paths call
//! [`run`], so they can never drift apart.
//!
//! There is deliberately no command to change the session mode. The read-only
//! guarantee rests on the mode, so letting chat change it would defeat the whole
//! arrangement; `/model` changes only which model answers.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::config::DiscordSettings;
use crate::grip::Grip;

use super::{issue_code, paired_users, policy, session_for};

/// Who invoked a command, independent of whether it came as a message or an
/// interaction. Only the identity and the privacy of the channel matter.
pub struct Invoker {
    pub user_id: String,
    pub is_dm: bool,
}

/// Which Thetis account a command's invoker is, or a synthetic per-channel
/// owner when the snowflake is bound to nothing.
///
/// Every command that touches a conversation goes through this rather than
/// using the snowflake or the conversation key, because the account is what
/// authority is resolved from and what fork routing compares. The synthetic
/// fallback is not an account — it carries the read-only Discord policy and
/// names no principal — but it is a stable id, which is what the caller needs.
fn account_of(grip: &Arc<Grip>, invoker: &Invoker, key: &str) -> String {
    grip.cfg()
        .auth
        .owner_for_discord(&invoker.user_id, &format!("discord:{key}"))
}

/// One command as Discord needs to be told about it.
struct Spec {
    name: &'static str,
    description: &'static str,
    /// A single optional free-form argument, when the command takes one.
    argument: Option<(&'static str, &'static str)>,
}

/// The commands offered in the picker, and the source of `/help`.
///
/// `mode` is not here on purpose: it exists in [`run`] only to refuse politely
/// if someone types it, and advertising it would invite the attempt.
const SPECS: &[Spec] = &[
    Spec {
        name: "new",
        description: "Start a fresh conversation here",
        argument: None,
    },
    Spec {
        name: "stop",
        description: "Interrupt what I am doing",
        argument: None,
    },
    Spec {
        name: "status",
        description: "Session, model and last-turn cost",
        argument: None,
    },
    Spec {
        name: "model",
        description: "Show or change the model answering here",
        argument: Some(("id", "Model id, from /models. Omit to see the current one")),
    },
    Spec {
        name: "models",
        description: "List the models available",
        argument: None,
    },
    Spec {
        name: "pair",
        description: "Issue a code authorising someone new (admins, in a DM)",
        argument: None,
    },
    Spec {
        name: "whoami",
        description: "Your id and what you are allowed to do",
        argument: None,
    },
    Spec {
        name: "help",
        description: "List the commands",
        argument: None,
    },
];

/// Commands advertised only when the operator has enabled them.
///
/// `/fork` is here rather than in [`SPECS`] because a command in the picker is
/// a promise. Advertising one that always answers "the operator has not enabled
/// this" teaches people the bot is broken, and — worse for something that
/// concerns permissions — it advertises the existence of an escalation path on
/// every install that has deliberately not opened one.
const OPTIONAL_SPECS: &[(&str, Spec)] = &[(
    "fork",
    Spec {
        name: "fork",
        description: "Talk to a conversation running under your own permissions",
        argument: Some(("state", "Say `off` to go back to the read-only conversation")),
    },
)];

/// The commands to register, given what is switched on.
fn specs_for(cfg: &DiscordSettings) -> Vec<&'static Spec> {
    let mut specs: Vec<&'static Spec> = SPECS.iter().collect();
    for (name, spec) in OPTIONAL_SPECS {
        let enabled = match *name {
            "fork" => cfg.allow_fork,
            // A new optional command with no switch would silently never be
            // advertised, which is a confusing way to find out about a missing
            // arm. Refuse to guess.
            other => unreachable!("no switch is wired up for the optional command /{other}"),
        };
        if enabled {
            specs.push(spec);
        }
    }
    specs
}

/// The payload for a bulk overwrite of the application's global commands.
///
/// Both context fields are named explicitly, and that is load-bearing.
///
/// Omitting them is the obvious-looking choice — the documentation says each
/// command then inherits the application's configured contexts, and `contexts`
/// is documented to default to every interaction context. Discord does not
/// behave that way. A bulk overwrite that leaves them out stores
/// `contexts: null`, which is not "all contexts": the command lands in a limbo
/// state where the client's picker never offers it. Nothing is rejected, the
/// PUT answers 200 with all eight commands echoed back, and the connector logs
/// a successful registration — but no `INTERACTION_CREATE` ever arrives,
/// because the client never lets the invocation be sent. That is exactly what
/// "the slash commands do not work" looks like from a guild, and it is
/// indistinguishable from a propagation delay until the stored objects are read
/// back. Discord acknowledges the mismatch as a bug in their own tracker
/// (discord-api-docs #7108, #6744, #7396), fixed only for commands created
/// after the fix and never backfilled, so anything registered before it stays
/// broken until a write names the fields.
///
/// - `integration_types: [GUILD_INSTALL]` — guild install only. User install
///   (`1`) would make this agent invocable in any DM or server the *installing
///   user* can reach, including ones the operator has no part in. The connector
///   authorizes by user id, so that is not an authentication hole, but it is a
///   surface with no reason to exist.
/// - `contexts: [GUILD, BOT_DM]` — the two places this connector actually
///   answers. `PRIVATE_CHANNEL` (`2`) is omitted because it is only meaningful
///   for a user-installed command, which this is not.
///
/// The earlier warning that naming these fields risks an "Unknown integration"
/// failure was the wrong lesson from a real failure: that happens when a
/// command asks for an installation context the *application* does not have
/// enabled. Asking only for guild install, which every application supports,
/// cannot hit it. Verified live: the PUT answers 200 and Discord echoes
/// `contexts: [0, 1]` and `integration_types: [0]` on all eight commands.
pub fn schema(cfg: &DiscordSettings) -> Value {
    const CHAT_INPUT: u64 = 1;
    const STRING_OPTION: u64 = 3;
    /// Installation context: the app was installed to a server.
    const GUILD_INSTALL: u64 = 0;
    /// Interaction contexts: a guild channel, and a DM with the bot itself.
    const CONTEXT_GUILD: u64 = 0;
    const CONTEXT_BOT_DM: u64 = 1;
    Value::Array(
        specs_for(cfg)
            .into_iter()
            .map(|spec| {
                let mut command = json!({
                    "name": spec.name,
                    "type": CHAT_INPUT,
                    "description": spec.description,
                    "integration_types": [GUILD_INSTALL],
                    "contexts": [CONTEXT_GUILD, CONTEXT_BOT_DM],
                });
                if let Some((name, description)) = spec.argument {
                    command["options"] = json!([{
                        "name": name,
                        "type": STRING_OPTION,
                        "description": description,
                        "required": false,
                    }]);
                }
                command
            })
            .collect(),
    )
}

/// Splits a typed line such as `/model gpt-5` into a name and an argument.
///
/// `None` means the text was not a command at all and belongs to the agent.
pub fn parse_typed(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let name = parts.next()?;
    Some((name.to_lowercase(), parts.collect::<Vec<_>>().join(" ")))
}

/// Runs a command. `Ok(None)` means the name is not one of ours — for the text
/// path that means the words go to the agent instead.
pub async fn run(
    grip: &Arc<Grip>,
    cfg: &DiscordSettings,
    invoker: &Invoker,
    key: &str,
    name: &str,
    argument: &str,
) -> Result<Option<String>> {
    let reply = match name {
        "help" => {
            let list = specs_for(cfg)
                .into_iter()
                .map(|s| format!("`/{}` — {}", s.name, s.description))
                .collect::<Vec<_>>()
                .join("\n");
            // The second paragraph has to stay true when forking is on. The
            // guarantee is unchanged — *this* conversation cannot change
            // anything, and no command can make it — but saying nothing more
            // would misdescribe an instance where `/fork` exists.
            let caveat = if cfg.allow_fork {
                "\n\n`/fork` does not change that. It moves your messages to a \
                 *different* conversation, one that runs under the permissions \
                 your own Thetis account already has. This one stays read-only, \
                 and so does everyone else's."
            } else {
                ""
            };
            format!(
                "**Commands**\n{list}\n\n\
                 I am in **{}** mode: I can read and research, but I cannot change \
                 anything on this machine. That is enforced by the grip, not by \
                 me, and there is no command to change it.{caveat}",
                cfg.mode
            )
        }

        "new" => {
            // Shared with the first-message path deliberately: this is where a
            // conversation's ceiling is stamped, and a second copy of the
            // creation code is a second place to forget it.
            super::new_session_for(grip, key, &invoker.user_id).await?;
            let account = account_of(grip, invoker, key);
            // `/new` resets the read-only conversation. It deliberately does not
            // touch a bound fork: that one has more authority and may be
            // mid-task, so discarding it has to be asked for in as many words.
            let forked = super::fork_for(grip, key)
                .await?
                .is_some_and(|(_, owner)| policy::may_prompt_fork(&owner, &account));
            if forked {
                "Started a fresh read-only conversation. Your messages are still \
                 going to your fork, though — say `/fork off` first if you meant \
                 to talk to this one."
                    .to_string()
            } else {
                "Started a fresh conversation. I have forgotten what came before.".to_string()
            }
        }

        "stop" => {
            // Routed, like /status: stopping the read-only conversation while
            // the speaker's fork carries on working is the worst possible
            // answer to someone typing /stop.
            let account = account_of(grip, invoker, key);
            let session_id = super::route_for(grip, key, &account, &invoker.user_id).await?;
            if grip.cancel(&session_id).await {
                "Stopping.".to_string()
            } else {
                "Nothing was running.".to_string()
            }
        }

        "status" => {
            // The conversation *this speaker* is talking to, which is their
            // fork if they have one bound here. Reporting the read-only one
            // while their messages went elsewhere would be a lie about the very
            // thing someone runs /status to find out.
            let account = account_of(grip, invoker, key);
            let session_id = super::route_for(grip, key, &account, &invoker.user_id).await?;
            let meta = grip
                .persist
                .get_session(&session_id)
                .await?
                .ok_or_else(|| anyhow!("the session vanished"))?;
            let model = if meta.model.is_empty() {
                grip.cfg().model.clone()
            } else {
                meta.model.clone()
            };
            let spend = grip.persist.get_spend(&session_id).await.unwrap_or(0.0);
            // Read off the stored ceiling, not the mode. The mode only filters
            // the tool list inside a component this instance can rewrite; the
            // ceiling is what `host_api::require` actually enforces, so it is
            // the honest answer to "what can this thing do".
            let authority = match grip.persist.ceiling_of(&session_id).await {
                Ok(Some(c)) if c.read_only => "read-only".to_string(),
                Ok(Some(_)) => format!("your own permissions, as `{account}`"),
                // No ceiling row means nothing narrows the speaker. That should
                // not happen for a conversation reachable from Discord, so say
                // so rather than implying a bound that is not there.
                Ok(None) => "unbounded (unexpected here — tell an operator)".to_string(),
                Err(_) => "unknown".to_string(),
            };
            format!(
                "**Session** `{}`\n**Mode** {}\n**Can do** {authority}\n**Model** {}\n\
                 **Events** {}\n**Spent** ${:.4}",
                meta.id, meta.mode, model, meta.event_count, spend
            )
        }

        "models" => {
            let list = grip
                .cfg()
                .models
                .iter()
                .map(|m| format!("• `{}` — {}", m.id, m.label))
                .collect::<Vec<_>>()
                .join("\n");
            if list.is_empty() {
                "No models are configured.".to_string()
            } else {
                format!("**Models**\n{list}")
            }
        }

        "model" => {
            // Routed, like /status and /stop: someone changing the model means
            // the conversation they are speaking to.
            let account = account_of(grip, invoker, key);
            let session_id = super::route_for(grip, key, &account, &invoker.user_id).await?;
            if argument.is_empty() {
                let meta = grip.persist.get_session(&session_id).await?;
                let current = meta
                    .map(|m| m.model)
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| grip.cfg().model.clone());
                format!("Model: `{current}`. Give me an id to change it, or /models to see them.")
            } else if grip.cfg().models.iter().any(|m| m.id == argument) {
                grip.persist.set_model(&session_id, argument).await?;
                format!("Model set to `{argument}`.")
            } else {
                format!("`{argument}` is not one of the configured models. Try /models.")
            }
        }

        "pair" => {
            if !cfg.is_admin(&invoker.user_id) {
                "Only an administrator can issue a pairing code.".to_string()
            } else if !invoker.is_dm {
                // A code posted in a channel is a code everyone present can use.
                "Ask me for that in a direct message, so the code stays private.".to_string()
            } else {
                let code = issue_code(grip, &invoker.user_id).await;
                format!(
                    "Pairing code: **{code}**\nIt is valid for {} minutes. Have them \
                     send it to me in a direct message.",
                    cfg.pairing_code_ttl.as_secs() / 60
                )
            }
        }

        "whoami" => {
            let paired = paired_users(grip).await;
            let account = account_of(grip, invoker, key);
            // Which Thetis account this snowflake is bound to, if any — the
            // thing that decides what a fork could do. An unbound identity is
            // reported as unlinked rather than as its synthetic owner id, which
            // would read like an account and is not one.
            let linked = match policy::may_fork(cfg, &account) {
                Ok(()) => format!("`{account}` — `/fork` will run as you"),
                Err(policy::ForkRefusal::Unbound) => {
                    "not linked to a Thetis account".to_string()
                }
                Err(policy::ForkRefusal::Disabled) if account.starts_with("discord:") => {
                    "not linked to a Thetis account".to_string()
                }
                Err(policy::ForkRefusal::Disabled) => {
                    format!("`{account}` (forking is not enabled here)")
                }
            };
            format!(
                "Your Discord id is `{}`.\nAuthorised: {}\nAdministrator: {}\n\
                 Thetis account: {linked}\nThis conversation is `{}`.",
                invoker.user_id,
                cfg.authorized(&invoker.user_id, &paired),
                cfg.is_admin(&invoker.user_id),
                key
            )
        }

        // A mode switch is the one thing chat must not be able to do. Answered
        // rather than ignored, so the refusal is unambiguous.
        "mode" => "There is no mode command. This surface is read-only by design.".to_string(),

        "fork" => fork(grip, cfg, invoker, key, argument).await?,

        _ => return Ok(None),
    };

    Ok(Some(reply))
}

/// `/fork` — start, or hand back to, a conversation that runs under the
/// invoker's own permissions.
///
/// The read-only conversation in this channel is left exactly as it was. What
/// changes is only where *this person's* messages go: a fork is bound to the
/// conversation key, and `super::route_for` sends a message to it when the
/// speaker is the account that authorised it and to the ordinary read-only
/// conversation otherwise. So the write-enabled conversation stays promptable —
/// which is the point, a fork you cannot follow up with is a fire-and-forget
/// wish — while nobody else in the channel can address it.
///
/// Authority itself does not depend on that routing. Every turn runs under
/// `policy(speaker) ∩ ceiling(session)`, so even a message that somehow reached
/// the fork from another account would be executed with *that* account's
/// permissions, narrowed by the fork's ceiling. The routing rule is about
/// coherence — one conversation, one authority, legible refusals — not about
/// being the thing that stops an escalation.
async fn fork(
    grip: &Arc<Grip>,
    cfg: &DiscordSettings,
    invoker: &Invoker,
    key: &str,
    argument: &str,
) -> Result<String> {
    // The resolved account, never the snowflake and never the channel. With
    // `group_sessions_per_user = false` a channel's key carries no user id, so
    // anything keyed on the conversation would authorise the whole channel.
    let account = account_of(grip, invoker, key);

    if let Err(refusal) = policy::may_fork(cfg, &account) {
        return Ok(match refusal {
            policy::ForkRefusal::Disabled => "Forking is not enabled on this instance. \
                 An operator has to set `discord.allow_fork`, because it is the one \
                 thing here that can start work which changes this machine."
                .to_string(),
            policy::ForkRefusal::Unbound => "Your Discord account is not linked to a Thetis \
                 account, so there are no permissions of yours to run under. An \
                 administrator links them with `discord_id` on your user entry."
                .to_string(),
        });
    }

    if argument.trim().eq_ignore_ascii_case("off") {
        // Only your own binding. Unbinding is not destructive, but it is not
        // nothing either: it decides where a *different* account's messages go,
        // and someone whose fork was quietly detached would have their next
        // message answered by a conversation with none of the context or the
        // authority they were relying on.
        let Some((_, owner)) = super::fork_for(grip, key).await? else {
            return Ok("You were not talking to a fork.".to_string());
        };
        if !policy::may_prompt_fork(&owner, &account) {
            return Ok(format!(
                "The fork here belongs to `{owner}`, so it is not yours to put \
                 away. You are already talking to the read-only conversation."
            ));
        }
        super::forget_fork(grip, key).await?;
        return Ok("Back to the read-only conversation. The fork is still there in \
             the web UI, and `/fork` will start a new one."
            .to_string());
    }

    if let Some((existing, owner)) = super::fork_for(grip, key).await? {
        // Someone else's fork is bound here. Do not replace it — that would let
        // one person quietly take over another's in-flight work — and do not
        // join it either.
        if !policy::may_prompt_fork(&owner, &account) {
            return Ok(format!(
                "There is already a fork here, authorised by a different account \
                 (`{owner}`). Only they can talk to it. Your messages are going to \
                 the read-only conversation."
            ));
        }
        return Ok(format!(
            "You are already talking to your fork, conversation `{existing}`. \
             Say `/fork off` to go back to the read-only conversation."
        ));
    }

    let parent = session_for(grip, key, &invoker.user_id).await?;
    let id = super::fork_session_for(grip, key, &account, Some(&parent)).await?;
    Ok(format!(
        "Talking to a fork under your own permissions, as `{account}`.\n\
         Conversation `{id}` — it is in the web UI, and it is the same one every \
         time you speak here until you say `/fork off`.\n\
         Everyone else in this channel still gets the read-only conversation."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings with everything optional switched off, which is the shape an
    /// install has unless an operator has decided otherwise.
    fn settings() -> DiscordSettings {
        crate::discord::policy::tests::settings()
    }

    /// Settings with every optional command enabled, for asserting the ones
    /// that only exist when switched on are still well-formed.
    fn everything() -> DiscordSettings {
        let mut cfg = settings();
        cfg.allow_fork = true;
        cfg
    }

    /// Every command name `run` has an arm for, advertised or not.
    const HANDLED: &[&str] = &[
        "help", "new", "stop", "status", "models", "model", "pair", "whoami", "mode", "fork",
    ];

    #[test]
    fn every_advertised_command_is_handled() {
        // A command in the picker that `run` does not know would answer "not a
        // command I know" — worse than not offering it. Optional commands are
        // checked too: being conditionally advertised is no excuse for being
        // unhandled, since the condition is the operator's to flip.
        for spec in SPECS.iter().chain(OPTIONAL_SPECS.iter().map(|(_, s)| s)) {
            assert!(
                HANDLED.contains(&spec.name),
                "/{} is registered but not handled",
                spec.name
            );
        }
    }

    #[test]
    fn an_optional_command_is_absent_until_it_is_switched_on() {
        // Advertising something that always refuses teaches people the bot is
        // broken, and for /fork specifically it would announce an escalation
        // path on every install that deliberately has none.
        let off = schema(&settings());
        let names: Vec<&str> = off
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"fork"), "got {names:?}");

        let on = schema(&everything());
        let names: Vec<&str> = on
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"fork"), "got {names:?}");
    }

    #[test]
    fn an_optional_command_obeys_the_same_registration_rules() {
        // The context fields are what make a command invocable at all, and an
        // optional command takes a different path through `specs_for` — so the
        // rules are asserted over the enabled-everything schema too, not just
        // the default one.
        for command in schema(&everything()).as_array().unwrap() {
            let name = command["name"].as_str().unwrap();
            assert_eq!(command["contexts"], json!([0, 1]), "/{name}");
            assert_eq!(command["integration_types"], json!([0]), "/{name}");
            let description = command["description"].as_str().unwrap();
            assert!((1..=100).contains(&description.chars().count()), "/{name}");
            assert_eq!(name, name.to_lowercase(), "/{name}");
        }
    }

    #[test]
    fn every_optional_command_has_a_switch_wired_up() {
        // `specs_for` panics on an optional command with no switch, rather than
        // silently never advertising it. This is the test that turns that panic
        // into a compile-time-ish guarantee instead of a production surprise.
        let _ = specs_for(&settings());
        let _ = specs_for(&everything());
    }

    #[test]
    fn the_mode_command_is_never_advertised() {
        assert!(
            SPECS.iter().all(|s| s.name != "mode"),
            "offering /mode in the picker invites the one thing chat must not do"
        );
    }

    #[test]
    fn the_schema_satisfies_discords_naming_rules() {
        let commands = schema(&settings());
        let commands = commands.as_array().expect("an array of commands");
        assert_eq!(commands.len(), SPECS.len());
        for command in commands {
            let name = command["name"].as_str().unwrap();
            // Lowercase, 1-32 characters, and no spaces.
            assert!(
                (1..=32).contains(&name.chars().count()),
                "{name} is the wrong length"
            );
            assert_eq!(name, name.to_lowercase(), "{name} must be lowercase");
            assert!(!name.contains(' '), "{name} must not contain a space");
            // Descriptions are required for CHAT_INPUT and capped at 100.
            let description = command["description"].as_str().unwrap();
            assert!(
                (1..=100).contains(&description.chars().count()),
                "the description of {name} is the wrong length"
            );
            assert_eq!(command["type"], 1);
        }
    }

    #[test]
    fn every_command_names_its_contexts_explicitly() {
        // The bug this pins down: omitting these fields makes Discord store
        // `contexts: null`, which is not the documented "all contexts" default.
        // The registration still answers 200, so the connector logs success
        // while the client's picker silently refuses to offer the command and
        // no INTERACTION_CREATE is ever sent. Leaving them out is what made
        // every slash command dead on a guild.
        for command in schema(&settings()).as_array().expect("an array of commands") {
            let name = command["name"].as_str().unwrap();
            assert_eq!(
                command["contexts"],
                json!([0, 1]),
                "/{name} must name GUILD and BOT_DM, or its picker entry never appears"
            );
            assert_eq!(
                command["integration_types"],
                json!([0]),
                "/{name} must be guild-install only; user install would let this \
                 agent be invoked anywhere the installing user can reach"
            );
        }
    }

    #[test]
    fn no_command_asks_for_the_user_install_context() {
        // Guarding the direction of the fix, not just its presence. Widening to
        // [0, 1] would "work" and would quietly add a surface: a user-installed
        // command travels with the person, into servers and DMs the operator
        // never approved.
        for command in schema(&settings()).as_array().unwrap() {
            let types = command["integration_types"].as_array().unwrap();
            assert!(
                !types.contains(&json!(1)),
                "/{} must not be user-installable",
                command["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn the_private_channel_context_is_not_requested() {
        // PRIVATE_CHANNEL (2) is only meaningful for a user-installed command.
        // Asking for it on a guild-install-only command is incoherent.
        for command in schema(&settings()).as_array().unwrap() {
            let contexts = command["contexts"].as_array().unwrap();
            assert!(!contexts.contains(&json!(2)));
        }
    }

    #[test]
    fn an_optional_argument_is_declared_as_an_optional_string() {
        let commands = schema(&settings());
        let model = commands
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "model")
            .expect("/model should be registered");
        let option = &model["options"][0];
        assert_eq!(option["type"], 3, "a free-form argument is a STRING option");
        assert_eq!(option["required"], false);
    }

    #[test]
    fn a_command_with_no_argument_declares_no_options() {
        let commands = schema(&settings());
        let new = commands
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "new")
            .unwrap();
        assert!(new.get("options").is_none());
    }

    #[test]
    fn a_typed_command_is_split_into_a_name_and_an_argument() {
        assert_eq!(
            parse_typed("/model gpt-5-mini"),
            Some(("model".into(), "gpt-5-mini".into()))
        );
        assert_eq!(parse_typed("/NEW"), Some(("new".into(), String::new())));
    }

    #[test]
    fn ordinary_text_is_not_a_command() {
        assert_eq!(parse_typed("what is new"), None);
        assert_eq!(parse_typed("/"), None);
    }
}
