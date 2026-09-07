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

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::DiscordSettings;
use crate::grip::Grip;

use super::{issue_code, paired_users, session_for, PAIR_SCOPE};

/// Who invoked a command, independent of whether it came as a message or an
/// interaction. Only the identity and the privacy of the channel matter.
pub struct Invoker {
    pub user_id: String,
    pub is_dm: bool,
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

/// The payload for a bulk overwrite of the application's global commands.
///
/// `integration_types` and `contexts` are deliberately omitted so each command
/// inherits whatever installation contexts the application is configured for.
/// Naming them looks more precise and is worse: asking for the user-install
/// context on an app that only has guild install makes Discord answer "Unknown
/// integration", and the registration fails wholesale. The default already
/// covers what this connector handles — guild channels and DMs with the bot.
pub fn schema() -> Value {
    const CHAT_INPUT: u64 = 1;
    const STRING_OPTION: u64 = 3;
    Value::Array(
        SPECS
            .iter()
            .map(|spec| {
                let mut command = json!({
                    "name": spec.name,
                    "type": CHAT_INPUT,
                    "description": spec.description,
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
            let list = SPECS
                .iter()
                .map(|s| format!("`/{}` — {}", s.name, s.description))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "**Commands**\n{list}\n\n\
                 I am in **{}** mode: I can read and research, but I cannot change \
                 anything on this machine. That is enforced by the grip, not by \
                 me, and there is no command to change it.",
                cfg.mode
            )
        }

        "new" => {
            let kv_key = format!("discord.session.{key}");
            let meta = grip
                .persist
                .create_session(Some(format!("Discord {key}")), &cfg.mode)
                .await?;
            grip.persist.kv_put(PAIR_SCOPE, &kv_key, &meta.id).await?;
            "Started a fresh conversation. I have forgotten what came before.".to_string()
        }

        "stop" => {
            let session_id = session_for(grip, key).await?;
            grip.cancel(&session_id).await;
            "Stopped.".to_string()
        }

        "status" => {
            let session_id = session_for(grip, key).await?;
            let meta = grip
                .persist
                .get_session(&session_id)
                .await?
                .ok_or_else(|| anyhow!("the session vanished"))?;
            let model = if meta.model.is_empty() {
                grip.cfg.model.clone()
            } else {
                meta.model.clone()
            };
            let spend = grip.persist.get_spend(&session_id).await.unwrap_or(0.0);
            format!(
                "**Session** `{}`\n**Mode** {} (read-only)\n**Model** {}\n\
                 **Events** {}\n**Spent** ${:.4}",
                meta.id, meta.mode, model, meta.event_count, spend
            )
        }

        "models" => {
            let list = grip
                .cfg
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
            let session_id = session_for(grip, key).await?;
            if argument.is_empty() {
                let meta = grip.persist.get_session(&session_id).await?;
                let current = meta
                    .map(|m| m.model)
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| grip.cfg.model.clone());
                format!("Model: `{current}`. Give me an id to change it, or /models to see them.")
            } else if grip.cfg.models.iter().any(|m| m.id == argument) {
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
            format!(
                "Your Discord id is `{}`.\nAuthorised: {}\nAdministrator: {}\n\
                 This conversation is `{}`.",
                invoker.user_id,
                cfg.authorized(&invoker.user_id, &paired),
                cfg.is_admin(&invoker.user_id),
                key
            )
        }

        // A mode switch is the one thing chat must not be able to do. Answered
        // rather than ignored, so the refusal is unambiguous.
        "mode" => "There is no mode command. This surface is read-only by design.".to_string(),

        _ => return Ok(None),
    };

    Ok(Some(reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_command_is_handled() {
        // A command in the picker that `run` does not know would answer "not a
        // command I know" — worse than not offering it.
        let handled = [
            "help", "new", "stop", "status", "models", "model", "pair", "whoami", "mode",
        ];
        for spec in SPECS {
            assert!(
                handled.contains(&spec.name),
                "/{} is registered but not handled",
                spec.name
            );
        }
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
        let commands = schema();
        let commands = commands.as_array().expect("an array of commands");
        assert_eq!(commands.len(), SPECS.len());
        for command in commands {
            let name = command["name"].as_str().unwrap();
            // Lowercase, 1-32 characters, and no spaces.
            assert!((1..=32).contains(&name.chars().count()), "{name} is the wrong length");
            assert_eq!(name, name.to_lowercase(), "{name} must be lowercase");
            assert!(!name.contains(' '), "{name} must not contain a space");
            // Descriptions are required for CHAT_INPUT and capped at 100.
            let description = command["description"].as_str().unwrap();
            assert!(
                (1..=100).contains(&description.chars().count()),
                "the description of {name} is the wrong length"
            );
            assert_eq!(command["type"], 1);
            // Installation contexts are left to the application's own
            // configuration; naming them can fail the whole registration.
            assert!(command.get("integration_types").is_none());
            assert!(command.get("contexts").is_none());
        }
    }

    #[test]
    fn an_optional_argument_is_declared_as_an_optional_string() {
        let commands = schema();
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
        let commands = schema();
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
