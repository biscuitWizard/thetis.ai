//! The Discord bot connector.
//!
//! Discord's gateway is an outbound WebSocket that must be held open and
//! heartbeated. A WebAssembly gateway component cannot do that: the `gateway`
//! world is only ever called in response to something arriving, a fresh
//! instance is made per call so nothing can be held open, and `wasi:http` has
//! no socket upgrade. So this lives in the orchestrator, and reaches the agent
//! through the same `submit` path the browser uses.
//!
//! ## The safety property
//!
//! Every session created here is stamped with a read-only mode, and the agent
//! withholds mutating tools for such a mode in two places: when it lists tools
//! for the model, and again at dispatch. This connector therefore adds no tool
//! policy of its own — it only has to make sure the mode is right and that
//! nothing exposed over Discord can change it. There is deliberately no command
//! to switch modes.

pub mod api;
pub mod ask;
pub mod commands;
pub mod policy;

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

use crate::bindings::types::SessionEvent;
use crate::grip::Grip;

use api::{Component, Event, Incoming, Interaction, Rest, Shard};
use policy::Decision;

/// KV scope and key holding the paired user ids.
const PAIR_SCOPE: &str = "global";
const PAIR_KEY: &str = "discord.paired_users";

/// The tool whose call is posted as controls rather than as a progress note.
/// Named here rather than in `ask` so the rendering module stays independent of
/// which tool happens to drive it.
const ASK_TOOL: &str = "ask_user";

/// Starts the connector, unless it is switched off or misconfigured.
///
/// Returns without spawning anything when there is no token, which is the
/// ordinary case for an install that does not use Discord.
pub fn spawn(grip: Arc<Grip>) -> Result<()> {
    let cfg = &grip.cfg.discord;

    if !cfg.enabled {
        tracing::debug!("the Discord connector is disabled");
        return Ok(());
    }
    let Some(token) = cfg.bot_token.clone() else {
        tracing::debug!("no Discord bot token; the connector stays off");
        return Ok(());
    };

    // The mode is the whole of the tool restriction, so verify it here rather
    // than trusting it. `read_only()` in the agent treats an unknown mode as
    // full access, so a typo would otherwise hand a public chat surface the dev
    // kit. Refuse to start instead, and say why.
    let mode = grip.cfg.mode(&cfg.mode).ok_or_else(|| {
        anyhow!(
            "the Discord connector is configured for mode '{}', which does not exist. \
             Refusing to start: an unknown mode would be treated as full access.",
            cfg.mode
        )
    })?;
    if !mode.read_only {
        return Err(anyhow!(
            "the Discord connector is configured for mode '{}', which is not read-only. \
             Refusing to start: that would expose tools that modify this machine over chat.",
            cfg.mode
        ));
    }

    if cfg.allow_all_users {
        tracing::warn!(
            "DISCORD_ALLOW_ALL_USERS is on: anyone who can see the bot may talk to it"
        );
    } else if cfg.allowed_users.is_empty() {
        tracing::warn!(
            "no Discord users are allowed yet; add ids to discord.allowed_users, \
             or an admin can use /pair once one is set"
        );
    }

    tracing::info!(mode = %cfg.mode, "starting the Discord connector");
    tokio::spawn(async move {
        if let Err(e) = run(grip, token.expose().to_string()).await {
            tracing::error!(error = %format!("{e:#}"), "the Discord connector stopped");
        }
    });
    Ok(())
}

/// Reconnect loop. A dropped socket is normal operation on Discord, not a
/// failure, so this backs off and resumes rather than giving up.
async fn run(grip: Arc<Grip>, token: String) -> Result<()> {
    let rest = Rest::new(token.clone(), grip.cfg.request_timeout)?;
    let mut backoff = Duration::from_secs(1);
    let mut resume: Option<(String, u64)> = None;
    let mut bot_id = String::new();
    // Commands only need registering once per process: a reconnect does not
    // clear them, and Discord rate-limits the endpoint globally.
    let mut commands_registered = false;
    // Likewise the rename: Discord allows roughly two username changes an hour,
    // so a reconnect loop must not spend that budget on every READY.
    let mut rename_attempted = false;

    loop {
        match Shard::connect(&token, resume.clone()).await {
            Ok(mut shard) => {
                loop {
                    match shard.next_event().await {
                        Ok(Event::Ready {
                            bot_id: id,
                            application_id,
                            bot_name,
                        }) => {
                            // Reset the backoff *here*, not on `connect`.
                            // `Shard::connect` returns as soon as the socket is
                            // open, before Discord has accepted the session, so
                            // a rejected IDENTIFY — a bad token, a shard limit —
                            // reconnected once a second forever instead of
                            // backing off.
                            backoff = Duration::from_secs(1);
                            if !id.is_empty() {
                                bot_id = id;
                            }
                            tracing::info!(bot = %bot_id, "connected to Discord");

                            // Show up under the configured agent name. Skipped
                            // when it already matches, because the rate limit
                            // here is about two changes an hour and a restart
                            // must not burn it re-setting the same name.
                            let wanted = grip.cfg.agent_name.clone();
                            if !rename_attempted && !wanted.is_empty() && bot_name != wanted {
                                rename_attempted = true;
                                match rest.set_username(&wanted).await {
                                    Ok(now) => tracing::info!(
                                        from = %bot_name,
                                        to = %now,
                                        "renamed the Discord bot to the configured agent name"
                                    ),
                                    // Never fatal: the bot answers perfectly
                                    // well under its old name, and the two
                                    // usual causes -- the hourly rate limit and
                                    // a name already taken -- are not something
                                    // retrying now would fix.
                                    Err(e) => tracing::warn!(
                                        error = %format!("{e:#}"),
                                        wanted = %wanted,
                                        current = %bot_name,
                                        "could not rename the Discord bot; it keeps its \
                                         current username. Discord allows about two \
                                         username changes an hour and rejects a name \
                                         already taken"
                                    ),
                                }
                            }

                            // Without this the Discord client has nothing to
                            // match `/new` against, so it refuses to send it
                            // and the bot never hears about it at all.
                            if !commands_registered && !application_id.is_empty() {
                                match rest
                                    .register_commands(&application_id, commands::schema())
                                    .await
                                {
                                    Ok(count) => {
                                        commands_registered = true;
                                        tracing::info!(
                                            count,
                                            "registered the Discord slash commands; \
                                             global commands can take up to an hour to \
                                             appear in a guild that was already joined"
                                        );
                                    }
                                    Err(e) => tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "could not register the Discord slash commands; \
                                         typed commands still work"
                                    ),
                                }
                            }
                        }
                        Ok(Event::Message(msg)) => {
                            // Each message is handled on its own task: a turn
                            // takes far longer than the heartbeat interval, and
                            // blocking here would kill the connection.
                            let h = grip.clone();
                            let r = rest.clone();
                            let bot = bot_id.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle(h, r, bot, msg).await {
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "failed to handle a Discord message"
                                    );
                                }
                            });
                        }
                        Ok(Event::Command(interaction)) => {
                            let h = grip.clone();
                            let r = rest.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_command(h, r, interaction).await {
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "failed to handle a Discord slash command"
                                    );
                                }
                            });
                        }
                        Ok(Event::Interacted(component)) => {
                            let h = grip.clone();
                            let r = rest.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_component(h, r, component).await {
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "failed to handle a Discord component"
                                    );
                                }
                            });
                        }
                        Ok(Event::Disconnected(why)) => {
                            tracing::warn!(%why, "the Discord socket dropped");
                            resume = shard.resume_state();
                            break;
                        }
                        Ok(Event::Fatal(why)) => {
                            tracing::error!(%why, "the Discord connector cannot continue");
                            return Err(anyhow!(why));
                        }
                        Err(e) => {
                            // A rejected token or a missing privileged intent
                            // will never succeed, however long we wait. Stop,
                            // and say what to change: retrying is what gets an
                            // address rate-limited.
                            if let Some(code) = shard.close_code() {
                                if api::is_fatal_close(code) {
                                    tracing::error!(
                                        code,
                                        advice = api::fatal_advice(code),
                                        "the Discord connector cannot continue"
                                    );
                                    return Err(anyhow!(
                                        "Discord closed the connection with {code}: {}",
                                        api::fatal_advice(code)
                                    ));
                                }
                            }
                            tracing::warn!(error = %format!("{e:#}"), "Discord gateway error");
                            resume = shard.resume_state();
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    backoff_secs = backoff.as_secs(),
                    "cannot reach the Discord gateway"
                );
            }
        }

        tokio::time::sleep(backoff).await;
        // Capped exponential backoff, so a long outage does not become a busy
        // loop against Discord's gateway.
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

// --- pairing ---------------------------------------------------------------

/// Users authorised by a pairing code, persisted so they survive a restart.
///
/// Stored in the existing scoped KV table rather than a new one: it is a short
/// list of ids, and a schema of its own would be more machinery than the data
/// deserves.
async fn paired_users(grip: &Grip) -> Vec<String> {
    grip
        .persist
        .kv_get(PAIR_SCOPE, PAIR_KEY)
        .await
        .ok()
        .flatten()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

async fn add_paired_user(grip: &Grip, user_id: &str) -> Result<()> {
    let mut users = paired_users(grip).await;
    if !users.iter().any(|u| u == user_id) {
        users.push(user_id.to_string());
    }
    grip
        .persist
        .kv_put(PAIR_SCOPE, PAIR_KEY, &users.join(","))
        .await
        .map_err(Into::into)
}

/// Outstanding pairing codes, with the moment each expires.
///
/// In memory only: a code is meant to be used within minutes, and one that did
/// not survive a restart is safer than one that did.
static CODES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (String, std::time::Instant)>>> =
    std::sync::OnceLock::new();

fn codes() -> &'static std::sync::Mutex<HashMap<String, (String, std::time::Instant)>> {
    CODES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// A short code that is easy to read aloud: no vowels, so it cannot spell
/// anything, and no characters that look alike in a chat font.
async fn new_code(grip: &Grip) -> String {
    const ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ23456789";
    let seed = grip
        .persist
        .kv_get(PAIR_SCOPE, "discord.code_counter")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let _ = grip
        .persist
        .kv_put(PAIR_SCOPE, "discord.code_counter", &(seed + 1).to_string())
        .await;

    let mut n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(seed)
        ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);

    let mut code = String::new();
    for _ in 0..6 {
        code.push(ALPHABET[(n % ALPHABET.len() as u64) as usize] as char);
        n /= ALPHABET.len() as u64;
    }
    code
}

async fn issue_code(grip: &Grip, issuer: &str) -> String {
    let code = new_code(grip).await;
    let expiry = std::time::Instant::now() + grip.cfg.discord.pairing_code_ttl;
    if let Ok(mut map) = codes().lock() {
        map.retain(|_, (_, exp)| *exp > std::time::Instant::now());
        map.insert(code.clone(), (issuer.to_string(), expiry));
    }
    code
}

/// Redeems a code, returning true when the user is now authorised.
async fn redeem_code(grip: &Grip, code: &str, user_id: &str) -> bool {
    let found = {
        let Ok(mut map) = codes().lock() else {
            return false;
        };
        map.retain(|_, (_, exp)| *exp > std::time::Instant::now());
        map.remove(&code.to_uppercase())
    };
    if found.is_none() {
        return false;
    }
    if let Err(e) = add_paired_user(grip, user_id).await {
        tracing::warn!(error = %e, "could not persist a paired user");
        return false;
    }
    tracing::info!(user = %user_id, "a Discord user was paired");
    true
}

// --- message handling ------------------------------------------------------

async fn handle(
    grip: Arc<Grip>,
    rest: Rest,
    bot_id: String,
    msg: Incoming,
) -> Result<()> {
    let cfg = grip.cfg.discord.clone();
    let paired = paired_users(&grip).await;

    // A pairing code is the one thing an unauthorized user may send, so it is
    // checked before the authorization decision.
    let trimmed = msg.content.trim().to_string();
    if !cfg.authorized(&msg.author_id, &paired)
        && msg.is_dm()
        && trimmed.len() <= 12
        && trimmed.chars().all(|c| c.is_ascii_alphanumeric())
        && !trimmed.is_empty()
        && redeem_code(&grip, &trimmed, &msg.author_id).await
    {
        rest.send_message(
            &msg.channel_id,
            "Paired. You can talk to me now. This surface is read-only: I can \
             read and research, but I cannot change anything on this machine.",
        )
        .await?;
        return Ok(());
    }

    match policy::decide(&cfg, &bot_id, &paired, &msg) {
        Decision::Answer => {}
        Decision::Ignore(why) => {
            tracing::trace!(%why, "ignoring a Discord message");
            return Ok(());
        }
        Decision::Unauthorized => {
            // Only answer in a DM. Saying this in a channel would be noise for
            // everyone else present.
            if msg.is_dm() {
                rest.send_message(
                    &msg.channel_id,
                    "I do not know you, so I will not answer. If you should have \
                     access, ask an administrator for a pairing code with /pair \
                     and send me the code.",
                )
                .await?;
            }
            tracing::info!(user = %msg.author_id, "refused an unauthorized Discord user");
            return Ok(());
        }
    }

    let text = policy::strip_mention(&msg.content, &bot_id);
    let key = policy::session_key(&cfg, &msg);

    if let Some(reply) = typed_command(&grip, &cfg, &msg, &key, &text).await? {
        rest.send_message(&msg.channel_id, &reply).await?;
        return Ok(());
    }

    if text.is_empty() {
        return Ok(());
    }

    let session_id = session_for(&grip, &key).await?;
    let attributed = policy::attribute(&msg, &text, cfg.group_sessions_per_user);

    // Subscribe before submitting, or a fast first token could be missed.
    let events = grip.events_tx.subscribe();
    grip.submit(&session_id, attributed, Vec::new()).await?;
    let _ = rest.typing(&msg.channel_id).await;

    let author_id = msg.author_id.clone();
    stream_reply(grip, rest, msg.channel_id, session_id, author_id, events).await
}

/// Runs a slash command that arrived as an interaction.
///
/// Authorization is repeated here rather than borrowed from the message path:
/// an interaction never passes through `decide`, so leaving it out would make
/// every command reachable by anyone who can see the bot.
async fn handle_command(
    grip: Arc<Grip>,
    rest: Rest,
    interaction: Interaction,
) -> Result<()> {
    let cfg = grip.cfg.discord.clone();
    let paired = paired_users(&grip).await;

    if !cfg.authorized(&interaction.user_id, &paired) {
        tracing::info!(user = %interaction.user_id,
            "refused a slash command from an unauthorized Discord user");
        // Ephemeral, so a refusal in a channel is not an announcement.
        rest.respond_to_interaction(
            &interaction.id,
            &interaction.token,
            "I do not know you, so I will not answer. If you should have access, \
             ask an administrator for a pairing code with /pair and send me the \
             code in a direct message.",
            true,
        )
        .await?;
        return Ok(());
    }

    let key = policy::session_key_for(
        &cfg,
        interaction.is_dm(),
        policy::is_thread_type(interaction.channel_type),
        &interaction.channel_id,
        &interaction.user_id,
    );

    let reply = commands::run(
        &grip,
        &cfg,
        &commands::Invoker {
            user_id: interaction.user_id.clone(),
            is_dm: interaction.is_dm(),
        },
        &key,
        &interaction.name,
        &interaction.argument,
    )
    .await?
    .unwrap_or_else(|| {
        format!("`/{}` is not a command I know. Try /help.", interaction.name)
    });

    // Discord shows "the application did not respond" after three seconds, and
    // every one of these commands is a database read or write, not a turn.
    rest.respond_to_interaction(&interaction.id, &interaction.token, &reply, true)
        .await
}

// --- asking the user -------------------------------------------------------

/// Epoch milliseconds, for a form's age.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A short id for one posted form.
///
/// Short on purpose: it travels inside a `custom_id`, which Discord caps at 100
/// characters alongside the question index and the action. Uniqueness only has
/// to hold among forms alive at once, and it is not a secret — the interaction
/// is authorized by user id, not by guessing this.
fn form_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Base36 of the low bits: eight characters, `[0-9a-z]`, no separators that
    // could be mistaken for the id's own delimiter.
    let mut id = String::new();
    let mut v = n;
    for _ in 0..8 {
        let digit = (v % 36) as u32;
        id.push(char::from_digit(digit, 36).unwrap_or('0'));
        v /= 36;
    }
    id
}

async fn load_form(grip: &Grip, state_id: &str) -> Option<ask::State> {
    let raw = grip
        .persist
        .kv_get(ask::SCOPE, &ask::key(state_id))
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

async fn save_form(grip: &Grip, state_id: &str, state: &ask::State) -> Result<()> {
    let raw = serde_json::to_string(state)?;
    grip.persist
        .kv_put(ask::SCOPE, &ask::key(state_id), &raw)
        .await
        .map_err(Into::into)
}

/// Drops a finished form's state.
///
/// Written empty rather than deleted: the KV interface has no delete, and an
/// empty value fails to deserialize, which `load_form` already reads as absent.
async fn clear_form(grip: &Grip, state_id: &str) {
    let _ = grip
        .persist
        .kv_put(ask::SCOPE, &ask::key(state_id), "")
        .await;
}

/// Posts the first question of an `ask_user` call.
///
/// Returns false when the call carried nothing askable, so the caller falls back
/// to its ordinary "… ask_user" progress note rather than posting an empty form.
async fn post_form(
    grip: &Grip,
    rest: &Rest,
    channel_id: &str,
    session_id: &str,
    user_id: &str,
    arguments_json: &str,
) -> bool {
    let Some(parsed) = ask::parse(arguments_json) else {
        tracing::warn!("an ask_user call carried no answerable questions");
        return false;
    };

    let state_id = form_id();
    let state = ask::State::new(session_id, channel_id, user_id, parsed, now_ms());

    // State before message: a form whose buttons arrive before their state can
    // be answered, and the answer would find nothing. The reverse — state with
    // no message — is merely a row that expires unread.
    if let Err(e) = save_form(grip, &state_id, &state).await {
        tracing::warn!(error = %format!("{e:#}"), "could not persist an ask_user form");
        return false;
    }

    match rest
        .send_with_components(
            channel_id,
            &ask::prompt_text(&state),
            ask::components(&state, &state_id),
        )
        .await
    {
        Ok(id) => {
            tracing::info!(session = %session_id, form = %state_id, message = %id,
                questions = state.ask.questions.len(), "posted an ask_user form");
            true
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "could not post an ask_user form");
            clear_form(grip, &state_id).await;
            false
        }
    }
}

/// Handles a press on one of the bot's own components.
///
/// Only `ask_user` forms use components, so anything else is left alone rather
/// than acknowledged: acknowledging an interaction we do not understand would
/// clear the "thinking" state on a control something else owns.
async fn handle_component(grip: Arc<Grip>, rest: Rest, component: Component) -> Result<()> {
    let Some(route) = ask::parse_custom_id(&component.custom_id) else {
        tracing::trace!(id = %component.custom_id, "a component that is not an ask_user form");
        return Ok(());
    };

    let Some(mut state) = load_form(&grip, &route.state_id).await else {
        // The form is gone: answered already, expired, or from a build before a
        // restart. Say so rather than failing silently — the buttons are still
        // on screen and pressing them has to mean something.
        return retire(
            &rest,
            &component,
            "This question is no longer open. Ask me again if you still want to answer.",
        )
        .await;
    };

    // Authorization is re-checked here, exactly as on the command path: a
    // component interaction never passes through `decide`, so without this
    // anyone who can see the message could answer in someone else's name.
    let paired = paired_users(&grip).await;
    if !grip.cfg.discord.authorized(&component.user_id, &paired) {
        tracing::info!(user = %component.user_id,
            "refused an ask_user interaction from an unauthorized Discord user");
        return rest
            .respond_to_interaction(
                &component.id,
                &component.token,
                "I do not know you, so I will not take your answer.",
                true,
            )
            .await;
    }
    // A form posted in a shared channel is addressed to the person whose turn
    // produced it. A bystander answering would put words in their mouth, and the
    // answer is submitted as *their* message.
    if state.user_id != component.user_id {
        return rest
            .respond_to_interaction(
                &component.id,
                &component.token,
                "These questions were asked of someone else, so they are not yours to answer.",
                true,
            )
            .await;
    }

    if state.expired(now_ms()) {
        clear_form(&grip, &route.state_id).await;
        return retire(
            &rest,
            &component,
            "These questions have expired — the turn that asked them is long finished.",
        )
        .await;
    }

    // A stale row: the message was edited on to the next question, but a client
    // that had not caught up pressed the old one. Applying it would record an
    // answer against the wrong question.
    if route.index != state.index {
        return rest
            .respond_to_interaction(
                &component.id,
                &component.token,
                "That was the previous question. Scroll to the latest message and answer there.",
                true,
            )
            .await;
    }

    let Some(question) = state.current().cloned() else {
        clear_form(&grip, &route.state_id).await;
        return retire(&rest, &component, "Every question is already answered.").await;
    };

    // Free text is the one branch that does not record an answer yet: Discord
    // has no text input on a message, so it opens a modal and the answer arrives
    // as a second interaction.
    if route.action == ask::Action::Other {
        return rest
            .open_modal(
                &component.id,
                &component.token,
                ask::modal(&state, &route.state_id),
            )
            .await;
    }

    let answer = match route.action {
        ask::Action::Skip => ask::Answer {
            skipped: true,
            text: String::new(),
        },
        ask::Action::Typed => {
            let typed = component.values.first().cloned().unwrap_or_default();
            if typed.trim().is_empty() {
                ask::Answer {
                    skipped: true,
                    text: String::new(),
                }
            } else {
                ask::Answer {
                    skipped: false,
                    text: typed.trim().to_string(),
                }
            }
        }
        ask::Action::Choose => match route.option {
            // A button press: the option is in the id, since a button sends no
            // values of its own.
            Some(index) => ask::answer_from_option(&question, index),
            // A select menu. Choosing the free-text entry from the menu asks for
            // the modal, exactly as the button does.
            None => match ask::answer_from_values(&question, &component.values) {
                Some(answer) => answer,
                None => {
                    return rest
                        .open_modal(
                            &component.id,
                            &component.token,
                            ask::modal(&state, &route.state_id),
                        )
                        .await;
                }
            },
        },
        ask::Action::Other => unreachable!("handled above"),
    };

    state.record(answer);
    let finished = state.done();
    let text = ask::prompt_text(&state);
    let components = ask::components(&state, &route.state_id);

    if finished {
        clear_form(&grip, &route.state_id).await;
    } else if let Err(e) = save_form(&grip, &route.state_id, &state).await {
        tracing::warn!(error = %format!("{e:#}"), "could not persist an ask_user answer");
    }

    /* A modal submission cannot be answered with an update to the message the
     * modal came from — Discord attaches no message to it — so that path
     * acknowledges the interaction and edits the message separately. A button or
     * menu press updates in one request, which is both fewer round trips and
     * atomic: the control cannot be pressed twice while the edit is in flight. */
    if component.from_modal {
        rest.ack_interaction(&component.id, &component.token).await?;
        if let Some(message_id) = component.message_id.as_deref() {
            if let Err(e) = rest
                .edit_with_components(&component.channel_id, message_id, &text, components)
                .await
            {
                tracing::warn!(error = %format!("{e:#}"),
                    "could not update an ask_user form after a modal");
            }
        }
    } else {
        rest.update_interaction_message(&component.id, &component.token, &text, components)
            .await?;
    }

    if !finished {
        return Ok(());
    }

    // Every question is answered, so the answers become an ordinary user
    // message — the same path the browser's form uses, and the same path typed
    // text uses. That is what puts them in the event log as something the model
    // simply reads.
    let answers = ask::compose(&state);
    let events = grip.events_tx.subscribe();
    grip.submit(&state.session_id, answers, Vec::new()).await?;
    let _ = rest.typing(&state.channel_id).await;
    stream_reply(
        grip,
        rest,
        state.channel_id.clone(),
        state.session_id.clone(),
        state.user_id.clone(),
        events,
    )
    .await
}

/// Answers an interaction whose form is gone, taking the dead controls away.
///
/// A modal submission has no message to update, so it gets an ephemeral note
/// instead; either way the press is acknowledged inside Discord's three seconds.
async fn retire(rest: &Rest, component: &Component, message: &str) -> Result<()> {
    if component.from_modal {
        return rest
            .respond_to_interaction(&component.id, &component.token, message, true)
            .await;
    }
    rest.update_interaction_message(
        &component.id,
        &component.token,
        message,
        serde_json::json!([]),
    )
    .await
}

/// Finds or creates the Thetis session backing a Discord conversation.
///
/// The mapping is persisted, so a channel keeps its history across restarts.
/// The mode is stamped at creation: this is the point where the read-only
/// guarantee is applied, and nothing exposed over Discord can undo it.
///
/// A conversation that is gone *or archived* is not reused. Archiving is how
/// someone says they are finished with a transcript: it shuts the worker down
/// and releases the checkout, so reviving it from chat would contradict an
/// explicit decision made in the web UI and quietly resurrect state that was
/// meant to be at rest. Discord gets a fresh conversation instead, exactly as
/// if the channel had never spoken. Archiving is therefore equivalent to `/new`
/// for every surface at once, and the archived transcript stays readable.
async fn session_for(grip: &Grip, key: &str) -> Result<String> {
    let kv_key = format!("discord.session.{key}");
    if let Some(existing) = grip.persist.kv_get(PAIR_SCOPE, &kv_key).await? {
        // `get_session` returns archived sessions too — archiving only sets a
        // flag, it does not delete — so the flag has to be read, not merely
        // the session's existence.
        let found = grip.persist.get_session(&existing).await?;
        if policy::may_reuse_session(found.as_ref().map(|m| m.archived)) {
            return Ok(existing);
        }
        match found {
            Some(_) => tracing::info!(session = %existing, %key,
                "the Discord conversation was archived; starting a fresh one"),
            None => tracing::info!(session = %existing, %key,
                "the Discord conversation is gone; starting a fresh one"),
        }
    }

    let title = format!("Discord {key}");
    let meta = grip
        .persist
        .create_session(Some(title), &grip.cfg.discord.mode).await?;
    grip.persist.kv_put(PAIR_SCOPE, &kv_key, &meta.id).await?;
    tracing::info!(session = %meta.id, %key, mode = %grip.cfg.discord.mode,
        "created a Discord session");
    Ok(meta.id)
}

/// Streams a turn's output into one Discord message, edited as it grows.
///
/// Discord rate-limits edits, so the text is buffered and the message is
/// updated on an interval rather than per token.
async fn stream_reply(
    grip: Arc<Grip>,
    rest: Rest,
    channel_id: String,
    session_id: String,
    // Who this turn is for. Needed so an `ask_user` form is addressed to the
    // person who spoke, rather than answerable by anyone in the channel.
    user_id: String,
    mut events: tokio::sync::broadcast::Receiver<crate::bindings::types::OutboundEvent>,
) -> Result<()> {
    let interval = grip.cfg.discord.stream_edit_interval;
    let mut buffer = String::new();
    let mut message_id: Option<String> = None;
    let mut last_edit = std::time::Instant::now();
    let mut last_sent = String::new();
    // Set once a question form has been posted, so the turn's ending does not
    // write a second message over the top of it.
    let mut asked = false;

    loop {
        let event = tokio::select! {
            received = events.recv() => received,
            // Refresh the typing indicator, which Discord clears after about
            // ten seconds, while a long turn is still working.
            _ = tokio::time::sleep(Duration::from_secs(8)) => {
                if buffer.is_empty() {
                    let _ = rest.typing(&channel_id).await;
                }
                continue;
            }
        };

        let event = match event {
            Ok(e) => e,
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "the Discord reader fell behind");
                continue;
            }
            Err(RecvError::Closed) => break,
        };

        if event.session_id != session_id {
            continue;
        }

        match event.event {
            SessionEvent::StreamDelta(chunk) => {
                buffer.push_str(&chunk);
                if last_edit.elapsed() >= interval && buffer.trim() != last_sent.trim() {
                    flush(&rest, &channel_id, &mut message_id, &buffer).await;
                    last_sent = buffer.clone();
                    last_edit = std::time::Instant::now();
                }
            }
            SessionEvent::AssistantMessage(m) => {
                // The final text is authoritative: streamed deltas can be
                // missing a tail, and a tool-only turn has no deltas at all.
                if !m.content.trim().is_empty() {
                    buffer = m.content.clone();
                }
            }
            SessionEvent::ToolInvocation(call) if call.name == ASK_TOOL => {
                /* The questions get their own message with real controls, rather
                 * than the "… ask_user" progress note. The turn is ending here —
                 * the agent's loop ends it as soon as this call succeeds — so
                 * anything said before the questions is flushed first, or it
                 * would be overwritten by the turn-finished text and lost. */
                if !buffer.trim().is_empty() {
                    flush(&rest, &channel_id, &mut message_id, &buffer).await;
                    last_sent = buffer.clone();
                }
                if post_form(
                    &grip,
                    &rest,
                    &channel_id,
                    &session_id,
                    &user_id,
                    &call.arguments_json,
                )
                .await
                {
                    // The form carries the conversation now. Leaving the buffer
                    // in place would make `TurnFinished` post "I finished
                    // without saying anything" underneath the questions.
                    asked = true;
                } else if buffer.trim().is_empty() {
                    // Nothing askable and nothing said: fall back to the note,
                    // so a malformed call is not silence.
                    let note = format!("_… {}_", call.name);
                    flush(&rest, &channel_id, &mut message_id, &note).await;
                    last_edit = std::time::Instant::now();
                }
            }
            SessionEvent::ToolInvocation(call) => {
                // Only shown while nothing has been said yet, so a long
                // research turn does not look stalled.
                if buffer.trim().is_empty() {
                    let note = format!("_… {}_", call.name);
                    if last_edit.elapsed() >= interval {
                        flush(&rest, &channel_id, &mut message_id, &note).await;
                        last_edit = std::time::Instant::now();
                    }
                }
            }
            SessionEvent::Incident(detail) => {
                buffer.push_str(&format!("\n\n**Something went wrong:** {detail}"));
            }
            SessionEvent::TurnFinished(stats) => {
                // A turn that ended by asking has already said its piece, in a
                // message with controls on it. Anything more would sit under the
                // questions contradicting them.
                if asked {
                    break;
                }
                if buffer.trim().is_empty() {
                    buffer = format!(
                        "I finished without saying anything (stopped by {}).",
                        stats.stopped_by
                    );
                }
                flush(&rest, &channel_id, &mut message_id, &buffer).await;
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

/// Sends the reply, or edits it if one is already posted.
async fn flush(
    rest: &Rest,
    channel_id: &str,
    message_id: &mut Option<String>,
    content: &str,
) {
    if content.trim().is_empty() {
        return;
    }
    match message_id {
        Some(id) => {
            if let Err(e) = rest.edit_message(channel_id, id, content).await {
                tracing::warn!(error = %format!("{e:#}"), "could not edit a Discord reply");
            }
        }
        None => match rest.send_message(channel_id, content).await {
            Ok(id) => *message_id = Some(id),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not send a Discord reply")
            }
        },
    }
}

// --- commands --------------------------------------------------------------

/// Handles a command typed as ordinary text, returning the reply to send.
///
/// `Ok(None)` means the text was not a command and should go to the agent.
///
/// Registered slash commands do not come this way — Discord turns those into
/// interactions, handled by `handle_command`. This path catches the same words
/// typed literally, which is what a client sees during the hour a freshly
/// registered global command takes to propagate. `commands::run` is shared, so
/// the two cannot answer differently.
async fn typed_command(
    grip: &Arc<Grip>,
    cfg: &crate::config::DiscordSettings,
    msg: &Incoming,
    key: &str,
    text: &str,
) -> Result<Option<String>> {
    let Some((name, argument)) = commands::parse_typed(text) else {
        return Ok(None);
    };
    let invoker = commands::Invoker {
        user_id: msg.author_id.clone(),
        is_dm: msg.is_dm(),
    };
    commands::run(grip, cfg, &invoker, key, &name, &argument).await
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    /// The connector's whole tool restriction is the mode it stamps, so a mode
    /// that is missing or not read-only must stop it from starting. The agent
    /// treats an unknown mode as full access, so failing open here would put
    /// the dev kit on a public chat surface.
    fn usable(cfg: &Config, mode_id: &str) -> bool {
        cfg.mode(mode_id).map(|m| m.read_only).unwrap_or(false)
    }

    /// Loads the shipped `thetis.toml`. `load()` resolves the root by walking
    /// up for the marker, and the crate directory is two levels below it, so no
    /// environment fiddling is needed.
    fn shipped() -> Config {
        Config::load().expect("the shipped config should load")
    }

    #[test]
    fn the_shipped_discord_mode_is_read_only() {
        let cfg = shipped();
        assert!(
            usable(&cfg, &cfg.discord.mode),
            "discord.mode ({}) must name a read-only mode",
            cfg.discord.mode
        );
    }

    #[test]
    fn a_missing_mode_would_not_be_usable() {
        assert!(!usable(&shipped(), "no-such-mode"));
    }

    #[test]
    fn a_writable_mode_would_not_be_usable() {
        // "agent" exists and is deliberately not read-only.
        assert!(!usable(&shipped(), "agent"));
    }

    #[test]
    fn the_bot_token_is_masked_in_the_settings_listing() {
        let cfg = shipped();
        let shown = crate::settings::list(&cfg, None).expect("settings should list");
        let entry = shown
            .iter()
            .find(|s| s.key == "discord.bot_token")
            .expect("discord.bot_token should be listed");
        // Empty in the shipped file; the masking itself is asserted by the
        // secrets test in `settings`, and the key is registered there.
        assert!(
            entry.value.is_empty() || entry.value == "***",
            "a token must never be shown, got {:?}",
            entry.value
        );
    }
}
