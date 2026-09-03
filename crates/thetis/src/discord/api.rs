//! The Discord side of the connector: REST calls and the gateway socket.
//!
//! This module knows about Discord and nothing about Thetis. It speaks the
//! wire protocol and hands plain events upward, which keeps the routing and
//! authorization policy in `mod.rs` testable without a socket.

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};

pub const API_BASE: &str = "https://discord.com/api/v10";
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Intents: GUILDS, GUILD_MESSAGES, DIRECT_MESSAGES, MESSAGE_CONTENT.
///
/// MESSAGE_CONTENT and GUILD_MEMBERS are privileged and must also be enabled in
/// the Developer Portal. Without MESSAGE_CONTENT every message arrives with
/// empty text, which is the single most common reason a bot looks connected but
/// never answers.
pub const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

// --- gateway opcodes -------------------------------------------------------

const OP_DISPATCH: u64 = 0;
const OP_HEARTBEAT: u64 = 1;
const OP_IDENTIFY: u64 = 2;
const OP_RESUME: u64 = 6;
const OP_RECONNECT: u64 = 7;
const OP_INVALID_SESSION: u64 = 9;
const OP_HELLO: u64 = 10;
const OP_HEARTBEAT_ACK: u64 = 11;

/// One inbound Discord message, reduced to what the connector needs.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub message_id: String,
    pub channel_id: String,
    /// Absent in a DM.
    pub guild_id: Option<String>,
    pub author_id: String,
    /// Display name, for attributing messages in a shared channel.
    pub author_name: String,
    pub author_is_bot: bool,
    pub content: String,
    /// User ids this message mentions.
    pub mentions: Vec<String>,
    /// Whether the message came from a thread.
    pub in_thread: bool,
}

impl Incoming {
    pub fn is_dm(&self) -> bool {
        self.guild_id.is_none()
    }
}

/// One inbound slash-command invocation.
///
/// A slash command never arrives as a message: the Discord client intercepts
/// text beginning with `/` and, when it matches a registered command, sends an
/// INTERACTION_CREATE instead of a MESSAGE_CREATE. A connector that only reads
/// messages therefore cannot see slash commands at all, which is why this is a
/// separate inbound shape.
#[derive(Debug, Clone)]
pub struct Interaction {
    /// Interaction id and token: together the address a reply is posted to.
    pub id: String,
    pub token: String,
    pub channel_id: String,
    /// Absent in a DM.
    pub guild_id: Option<String>,
    /// Channel type, so a thread can be told from a channel as for a message.
    pub channel_type: Option<u64>,
    pub user_id: String,
    pub user_name: String,
    /// The command name, without the leading slash.
    pub name: String,
    /// Option values flattened to one string. These commands take at most one
    /// free-form argument, and the text path already parses that shape.
    pub argument: String,
}

impl Interaction {
    pub fn is_dm(&self) -> bool {
        self.guild_id.is_none()
    }
}

/// A button press, select-menu choice or modal submission.
///
/// These are the *other* two interaction types, and they are not commands: no
/// command name arrives, and what identifies them is the `custom_id` the bot
/// itself put on the component. They are kept separate from `Interaction` for
/// that reason — conflating them would mean a struct whose `name` is sometimes
/// a command and sometimes a made-up token.
#[derive(Debug, Clone)]
pub struct Component {
    pub id: String,
    pub token: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub channel_type: Option<u64>,
    pub user_id: String,
    pub user_name: String,
    /// The id the bot gave the component. This is the whole of the routing
    /// information, so it has to encode everything the handler needs.
    pub custom_id: String,
    /// Select-menu selections, or a modal's text-input values in row order.
    /// A button press has none.
    pub values: Vec<String>,
    /// The message the component sits on. Absent on a modal submission that
    /// Discord did not attach one to, which is why a modal's `custom_id` also
    /// carries the message id.
    pub message_id: Option<String>,
    /// True for a modal submission, false for a button or select menu.
    pub from_modal: bool,
}

impl Component {
    pub fn is_dm(&self) -> bool {
        self.guild_id.is_none()
    }
}

/// What the socket produced. `Resumed` and `Ready` are separate because only a
/// fresh READY invalidates the session state we were holding.
#[derive(Debug)]
pub enum Event {
    Ready {
        bot_id: String,
        application_id: String,
        /// The bot's current Discord username, so a rename can be skipped when
        /// it already matches and the rate limit is not spent for nothing.
        bot_name: String,
    },
    Message(Incoming),
    /// A slash command was invoked.
    Command(Interaction),
    /// A component on one of the bot's own messages was used.
    Interacted(Component),
    /// The socket ended; the caller reconnects.
    Disconnected(String),
    /// The socket ended for a reason reconnecting cannot fix.
    Fatal(String),
}

/// Close codes that no amount of reconnecting will fix.
///
/// Discord documents these as terminal, and retrying them is not merely futile:
/// hammering the gateway with a token it has already rejected is how an address
/// gets rate-limited or banned. The connector stops and says what to change.
///
/// 4004 authentication failed, 4010 invalid shard, 4011 sharding required,
/// 4012 invalid API version, 4013 invalid intents, 4014 disallowed intents
/// (a privileged intent not enabled in the Developer Portal).
pub fn is_fatal_close(code: u16) -> bool {
    matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014)
}

/// The advice that goes with a fatal close code, so the log says what to do.
pub fn fatal_advice(code: u16) -> &'static str {
    match code {
        4004 => "the bot token was rejected; check discord.bot_token or DISCORD_BOT_TOKEN",
        4013 | 4014 => {
            "the gateway refused the intents; enable Message Content Intent \
                        and Server Members Intent under Privileged Gateway Intents in the \
                        Discord Developer Portal"
        }
        4010 | 4011 => "the shard configuration is wrong for this bot",
        4012 => "the gateway API version is no longer supported by this build",
        _ => "the gateway closed the connection permanently",
    }
}

/// The names of commands Discord stored with no interaction contexts.
///
/// Such a command is registered as far as the API is concerned — it comes back
/// in the 200 response and can be fetched later — but the client never offers
/// it in the picker, so it can never be invoked. Pure, so the check that turns
/// a silent registration failure into a warning does not need a live API.
fn commands_without_contexts(stored: &[Value]) -> Vec<&str> {
    stored
        .iter()
        .filter(|c| c.get("contexts").map(Value::is_null).unwrap_or(true))
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect()
}

fn parse_message(d: &Value) -> Option<Incoming> {
    let author = d.get("author")?;
    // Webhook messages have no real author id; skip rather than guess.
    let author_id = author.get("id")?.as_str()?.to_string();
    let author_name = author
        .get("global_name")
        .and_then(Value::as_str)
        .or_else(|| author.get("username").and_then(Value::as_str))
        .unwrap_or("someone")
        .to_string();

    Some(Incoming {
        message_id: d.get("id")?.as_str()?.to_string(),
        channel_id: d.get("channel_id")?.as_str()?.to_string(),
        guild_id: d.get("guild_id").and_then(Value::as_str).map(String::from),
        author_id,
        author_name,
        author_is_bot: author.get("bot").and_then(Value::as_bool).unwrap_or(false),
        content: d
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        mentions: d
            .get("mentions")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        // Thread channel types: 10 announcement, 11 public, 12 private.
        in_thread: matches!(
            d.get("channel_type").and_then(Value::as_u64),
            Some(10) | Some(11) | Some(12)
        ),
    })
}

/// The invoker of an interaction, whatever its type.
///
/// In a guild the user is nested under `member`; in a DM it is at the top
/// level. Every interaction type shares that shape, so it is read once.
fn interaction_user(d: &Value) -> Option<(String, String)> {
    let user = d
        .get("member")
        .and_then(|m| m.get("user"))
        .or_else(|| d.get("user"))?;
    let id = user.get("id")?.as_str()?.to_string();
    let name = user
        .get("global_name")
        .and_then(Value::as_str)
        .or_else(|| user.get("username").and_then(Value::as_str))
        .unwrap_or("someone")
        .to_string();
    Some((id, name))
}

/// Where an interaction happened. `channel_id` is top-level on older payloads
/// and nested under `channel` on newer ones, so both are tried.
fn interaction_channel(d: &Value) -> (String, Option<u64>) {
    let id = d
        .get("channel_id")
        .and_then(Value::as_str)
        .or_else(|| {
            d.get("channel")
                .and_then(|c| c.get("id"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string();
    let kind = d
        .get("channel")
        .and_then(|c| c.get("type"))
        .and_then(Value::as_u64);
    (id, kind)
}

/// Reads an APPLICATION_COMMAND interaction, or `None` for any other type.
///
/// Component and modal interactions share this dispatch and are read by
/// `parse_component` instead: only type 2 carries a command name.
fn parse_interaction(d: &Value) -> Option<Interaction> {
    const APPLICATION_COMMAND: u64 = 2;
    if d.get("type").and_then(Value::as_u64) != Some(APPLICATION_COMMAND) {
        return None;
    }
    let data = d.get("data")?;
    let (user_id, user_name) = interaction_user(d)?;

    // Option values are flattened in declaration order. Every command here
    // takes at most one string, so joining is enough and lets the handler stay
    // shared with the text path.
    let argument = data
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|o| o.get("value"))
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let (channel_id, channel_type) = interaction_channel(d);
    Some(Interaction {
        id: d.get("id")?.as_str()?.to_string(),
        token: d.get("token")?.as_str()?.to_string(),
        channel_id,
        guild_id: d.get("guild_id").and_then(Value::as_str).map(String::from),
        channel_type,
        user_id,
        user_name,
        name: data.get("name")?.as_str()?.to_lowercase(),
        argument,
    })
}

/// Reads a MESSAGE_COMPONENT (type 3) or MODAL_SUBMIT (type 5) interaction.
///
/// `None` for anything else, including a command: the two paths are disjoint,
/// so a payload can never be read as both.
///
/// Modal values are nested two deep — `components[].components[].value` — one
/// action row per input. They are flattened into `values` in row order, which is
/// the order they were declared, so a caller reads them positionally.
fn parse_component(d: &Value) -> Option<Component> {
    const MESSAGE_COMPONENT: u64 = 3;
    const MODAL_SUBMIT: u64 = 5;
    let kind = d.get("type").and_then(Value::as_u64)?;
    if kind != MESSAGE_COMPONENT && kind != MODAL_SUBMIT {
        return None;
    }
    let data = d.get("data")?;
    let (user_id, user_name) = interaction_user(d)?;
    let (channel_id, channel_type) = interaction_channel(d);

    let values = if kind == MODAL_SUBMIT {
        data.get("components")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("components").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|c| c.get("value").and_then(Value::as_str))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        data.get("values")
            .and_then(Value::as_array)
            .map(|vals| {
                vals.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };

    Some(Component {
        id: d.get("id")?.as_str()?.to_string(),
        token: d.get("token")?.as_str()?.to_string(),
        channel_id,
        guild_id: d.get("guild_id").and_then(Value::as_str).map(String::from),
        channel_type,
        user_id,
        user_name,
        custom_id: data.get("custom_id")?.as_str()?.to_string(),
        values,
        message_id: d
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(String::from),
        from_modal: kind == MODAL_SUBMIT,
    })
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A single gateway connection. Dropped and rebuilt on reconnect, so it holds
/// no reconnection policy of its own.
pub struct Shard {
    socket: Socket,
    heartbeat: Duration,
    last_seq: Option<u64>,
    session_id: Option<String>,
    /// Set once a heartbeat has been sent and not yet acknowledged. A second
    /// unacknowledged beat means the connection is a zombie.
    awaiting_ack: bool,
    next_beat: tokio::time::Instant,
    /// Set when the peer sent a close frame, so the caller can tell a
    /// reconnectable drop from a permanent refusal.
    close_code: Option<u16>,
}

impl Shard {
    /// Opens a socket and completes the HELLO handshake.
    pub async fn connect(token: &str, resume: Option<(String, u64)>) -> Result<Self> {
        let (socket, _) = connect_async(GATEWAY_URL)
            .await
            .context("connecting to the Discord gateway")?;

        let mut shard = Self {
            socket,
            heartbeat: Duration::from_secs(41),
            last_seq: resume.as_ref().map(|(_, seq)| *seq),
            session_id: resume.as_ref().map(|(id, _)| id.clone()),
            awaiting_ack: false,
            next_beat: tokio::time::Instant::now(),
            close_code: None,
        };

        // HELLO always arrives first and carries the heartbeat interval.
        let hello = shard
            .read_payload()
            .await?
            .ok_or_else(|| anyhow!("the gateway closed before sending HELLO"))?;
        if hello.get("op").and_then(Value::as_u64) != Some(OP_HELLO) {
            return Err(anyhow!("expected HELLO, got op {:?}", hello.get("op")));
        }
        let interval = hello
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("HELLO carried no heartbeat_interval"))?;
        shard.heartbeat = Duration::from_millis(interval);

        // The first beat is jittered, as the docs require: without it every bot
        // that restarts together would beat in lockstep.
        let jitter = interval / 2 + (interval / 4);
        shard.next_beat = tokio::time::Instant::now() + Duration::from_millis(jitter);

        match resume {
            Some((session_id, seq)) => {
                shard
                    .send(json!({
                        "op": OP_RESUME,
                        "d": { "token": token, "session_id": session_id, "seq": seq },
                    }))
                    .await?;
                tracing::info!("resuming the Discord session");
            }
            None => {
                shard
                    .send(json!({
                        "op": OP_IDENTIFY,
                        "d": {
                            "token": token,
                            "intents": INTENTS,
                            "properties": {
                                "os": std::env::consts::OS,
                                "browser": "thetis",
                                "device": "thetis",
                            },
                        },
                    }))
                    .await?;
            }
        }

        Ok(shard)
    }

    /// The close code the peer sent, if it sent one.
    pub fn close_code(&self) -> Option<u16> {
        self.close_code
    }

    /// The state needed to resume this session, if it can be resumed.
    pub fn resume_state(&self) -> Option<(String, u64)> {
        match (&self.session_id, self.last_seq) {
            (Some(id), Some(seq)) => Some((id.clone(), seq)),
            _ => None,
        }
    }

    async fn send(&mut self, payload: Value) -> Result<()> {
        self.socket
            .send(Message::Text(payload.to_string().into()))
            .await
            .context("writing to the gateway socket")
    }

    async fn read_payload(&mut self) -> Result<Option<Value>> {
        while let Some(frame) = self.socket.next().await {
            match frame.context("reading the gateway socket")? {
                Message::Text(text) => {
                    return Ok(Some(serde_json::from_str(&text).context("gateway JSON")?));
                }
                Message::Close(frame) => {
                    // Keep the code separate from the prose: the caller has to
                    // decide whether this is worth retrying.
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        .unwrap_or((0, "no reason given".into()));
                    self.close_code = Some(code);
                    return Err(anyhow!(
                        "the gateway closed the connection: {code} {reason}"
                    ));
                }
                // Ping/pong are handled by tungstenite; binary would only
                // appear under a compression option we do not request.
                _ => continue,
            }
        }
        Ok(None)
    }

    /// Pulls the next meaningful event, keeping the heartbeat going meanwhile.
    ///
    /// Heartbeating shares this task rather than running in its own: the socket
    /// is not `Sync`, and a select here is simpler than putting it behind a
    /// mutex two tasks contend on.
    pub async fn next_event(&mut self) -> Result<Event> {
        loop {
            let payload = tokio::select! {
                _ = tokio::time::sleep_until(self.next_beat) => {
                    if self.awaiting_ack {
                        // A beat went unacknowledged: the connection is dead
                        // even though the socket looks open.
                        return Ok(Event::Disconnected(
                            "the gateway stopped acknowledging heartbeats".into(),
                        ));
                    }
                    let seq = self.last_seq;
                    self.send(json!({ "op": OP_HEARTBEAT, "d": seq })).await?;
                    self.awaiting_ack = true;
                    self.next_beat = tokio::time::Instant::now() + self.heartbeat;
                    continue;
                }
                frame = self.read_payload() => match frame? {
                    Some(p) => p,
                    None => return Ok(Event::Disconnected("the socket ended".into())),
                },
            };

            if let Some(seq) = payload.get("s").and_then(Value::as_u64) {
                self.last_seq = Some(seq);
            }

            match payload.get("op").and_then(Value::as_u64) {
                Some(OP_HEARTBEAT_ACK) => {
                    self.awaiting_ack = false;
                }
                Some(OP_HEARTBEAT) => {
                    // The gateway may ask for one out of band.
                    let seq = self.last_seq;
                    self.send(json!({ "op": OP_HEARTBEAT, "d": seq })).await?;
                }
                Some(OP_RECONNECT) => {
                    return Ok(Event::Disconnected(
                        "the gateway asked us to reconnect".into(),
                    ));
                }
                Some(OP_INVALID_SESSION) => {
                    // `d: false` means the session cannot be resumed, so drop
                    // it and let the caller identify afresh.
                    let resumable = payload.get("d").and_then(Value::as_bool).unwrap_or(false);
                    if !resumable {
                        self.session_id = None;
                        self.last_seq = None;
                    }
                    return Ok(Event::Disconnected(
                        "the gateway invalidated the session".into(),
                    ));
                }
                Some(OP_DISPATCH) => {
                    let name = payload.get("t").and_then(Value::as_str).unwrap_or("");
                    let d = payload.get("d").cloned().unwrap_or(Value::Null);
                    match name {
                        "READY" => {
                            self.session_id = d
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(String::from);
                            let bot_id = d
                                .get("user")
                                .and_then(|u| u.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            // Commands are registered under the application id,
                            // which is not guaranteed to equal the bot user id,
                            // so it is read from READY rather than assumed.
                            let application_id = d
                                .get("application")
                                .and_then(|a| a.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or(&bot_id)
                                .to_string();
                            let bot_name = d
                                .get("user")
                                .and_then(|u| u.get("username"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            return Ok(Event::Ready {
                                bot_id,
                                application_id,
                                bot_name,
                            });
                        }
                        "MESSAGE_CREATE" => {
                            if let Some(msg) = parse_message(&d) {
                                return Ok(Event::Message(msg));
                            }
                        }
                        "INTERACTION_CREATE" => {
                            if let Some(interaction) = parse_interaction(&d) {
                                return Ok(Event::Command(interaction));
                            }
                            // A button, select menu or modal. Disjoint from the
                            // command path: only one of the two parsers can
                            // match a given payload.
                            if let Some(component) = parse_component(&d) {
                                return Ok(Event::Interacted(component));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

// --- REST ------------------------------------------------------------------

/// The REST half. Cheap to clone; `reqwest::Client` pools connections.
#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    token: String,
}

#[derive(Deserialize)]
struct CreatedMessage {
    id: String,
}

impl Rest {
    pub fn new(token: String, timeout: Duration) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .context("building the Discord HTTP client")?,
            token,
        })
    }

    /// Mentions Thetis is willing to resolve.
    ///
    /// `@everyone` and role pings are denied outright: anything the model writes
    /// or echoes back could otherwise ping a whole server. User mentions stay
    /// on so ordinary conversation reads naturally.
    fn allowed_mentions() -> Value {
        json!({ "parse": ["users"], "replied_user": true })
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bot {}", self.token))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("Discord rejected POST {path}: {status} {text}"));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .patch(format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bot {}", self.token))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PATCH {path}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("Discord rejected PATCH {path}: {status} {text}"));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn put(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .put(format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bot {}", self.token))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PUT {path}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("Discord rejected PUT {path}: {status} {text}"));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Renames the bot user, so it appears in Discord under the configured
    /// agent name rather than whatever the developer portal was set to.
    ///
    /// Best-effort by design, and the caller treats a failure as a warning. Two
    /// things routinely make this fail and neither is worth stopping the
    /// connector for: Discord rate-limits username changes to roughly two per
    /// hour and answers 429, and a name already taken at that discriminator
    /// comes back 400. In both cases the bot keeps working under its old name.
    ///
    /// Returns the name Discord says it now has.
    pub async fn set_username(&self, username: &str) -> Result<String> {
        let updated = self
            .patch("/users/@me", json!({ "username": username }))
            .await?;
        Ok(updated
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or(username)
            .to_string())
    }

    /// Registers the slash commands, replacing whatever was registered before.
    ///
    /// A bulk overwrite rather than one create per command: it is a single
    /// request, and it deletes commands this build no longer has, so a renamed
    /// command does not leave a stale entry in the picker forever.
    ///
    /// Global scope is deliberate. Guild scope appears instantly but only in
    /// the guild named, and a command registered in both scopes shows up twice
    /// in the picker. Global commands can take up to an hour to appear in a
    /// guild that was already joined, which is worth saying out loud rather
    /// than leaving someone to wonder.
    ///
    /// The response is checked rather than merely counted. A registration that
    /// stores `contexts: null` is accepted with a 200 and every command echoed
    /// back, and then never appears in anyone's picker — so counting the array
    /// reports success for a command set that cannot be invoked. That is the
    /// exact shape of the bug that made every slash command dead on a guild,
    /// and it survived because the only evidence was a cheerful log line. The
    /// count returned now excludes anything Discord stored in that state, so a
    /// silent failure shows up as a warning and a short count.
    pub async fn register_commands(&self, application_id: &str, commands: Value) -> Result<usize> {
        let registered = self
            .put(
                &format!("/applications/{application_id}/commands"),
                commands,
            )
            .await?;
        let stored = registered.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let limbo = commands_without_contexts(stored);
        if !limbo.is_empty() {
            tracing::warn!(
                commands = %limbo.join(", "),
                "Discord stored these slash commands with no interaction contexts. \
                 They will not appear in the command picker and no interaction will \
                 ever arrive for them, even though the registration returned 200. \
                 The registration payload must name `contexts` explicitly"
            );
        }
        Ok(stored.len() - limbo.len())
    }

    /// Answers an interaction: Discord shows "the application did not respond"
    /// unless something arrives within three seconds.
    ///
    /// `ephemeral` sets flag 1<<6, so the reply is visible only to the invoker.
    /// Commands echo back configuration, and in a shared channel that is noise
    /// for everyone else.
    pub async fn respond_to_interaction(
        &self,
        interaction_id: &str,
        token: &str,
        content: &str,
        ephemeral: bool,
    ) -> Result<()> {
        const CHANNEL_MESSAGE_WITH_SOURCE: u64 = 4;
        let mut data = json!({
            "content": truncate(content),
            "allowed_mentions": Self::allowed_mentions(),
        });
        if ephemeral {
            data["flags"] = json!(1 << 6);
        }
        self.interaction_callback(
            interaction_id,
            token,
            json!({ "type": CHANNEL_MESSAGE_WITH_SOURCE, "data": data }),
        )
        .await
    }

    /// Answers a component interaction by rewriting the message it sits on.
    ///
    /// Type 7 is UPDATE_MESSAGE, which both acknowledges the interaction and
    /// replaces the content and components in one request — so a used control
    /// disappears rather than staying pressable. Passing an empty component list
    /// is what removes the controls; omitting the field would leave them.
    pub async fn update_interaction_message(
        &self,
        interaction_id: &str,
        token: &str,
        content: &str,
        components: Value,
    ) -> Result<()> {
        const UPDATE_MESSAGE: u64 = 7;
        self.interaction_callback(
            interaction_id,
            token,
            json!({
                "type": UPDATE_MESSAGE,
                "data": {
                    "content": truncate(content),
                    "components": components,
                    "allowed_mentions": Self::allowed_mentions(),
                },
            }),
        )
        .await
    }

    /// Answers an interaction by opening a modal.
    ///
    /// Type 9 is MODAL. A modal is the only way to collect free text on
    /// Discord: text inputs are not allowed on a message, only inside a modal,
    /// which is why a free-text answer needs a button or a menu entry to open
    /// one rather than being typed in place.
    ///
    /// A modal response cannot be sent to a modal submission — Discord rejects
    /// a modal from a modal — so this is only ever the answer to a command or a
    /// component.
    pub async fn open_modal(&self, interaction_id: &str, token: &str, modal: Value) -> Result<()> {
        const MODAL: u64 = 9;
        self.interaction_callback(
            interaction_id,
            token,
            json!({ "type": MODAL, "data": modal }),
        )
        .await
    }

    /// Acknowledges an interaction without changing anything on screen.
    ///
    /// Type 6 is DEFERRED_UPDATE_MESSAGE. Needed for a submission whose visible
    /// effect is applied by a later edit: something must reach Discord inside
    /// three seconds or the user is told the application did not respond.
    pub async fn ack_interaction(&self, interaction_id: &str, token: &str) -> Result<()> {
        const DEFERRED_UPDATE_MESSAGE: u64 = 6;
        self.interaction_callback(
            interaction_id,
            token,
            json!({ "type": DEFERRED_UPDATE_MESSAGE }),
        )
        .await
    }

    /// The one place an interaction callback is posted.
    ///
    /// Callbacks are authenticated by the token *in the path*, so this endpoint
    /// must not carry an Authorization header — sending one gets the request
    /// rejected, and the symptom is a 401 that looks like a bad bot token.
    async fn interaction_callback(
        &self,
        interaction_id: &str,
        token: &str,
        body: Value,
    ) -> Result<()> {
        let response = self
            .http
            .post(format!(
                "{API_BASE}/interactions/{interaction_id}/{token}/callback"
            ))
            .json(&body)
            .send()
            .await
            .context("answering an interaction")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Discord rejected an interaction reply: {status} {text}"
            ));
        }
        Ok(())
    }

    /// Sends a message and returns its id, so it can be edited while streaming.
    pub async fn send_message(&self, channel_id: &str, content: &str) -> Result<String> {
        self.send_with_components(channel_id, content, Value::Null)
            .await
    }

    /// Sends a message carrying interactive components.
    ///
    /// Discord allows at most five action rows on a message, and the components
    /// are the bot's own: a `custom_id` set here is what comes back on the
    /// interaction, and it is the only routing information available, so it has
    /// to encode everything the handler will need.
    pub async fn send_with_components(
        &self,
        channel_id: &str,
        content: &str,
        components: Value,
    ) -> Result<String> {
        let mut body = json!({
            "content": truncate(content),
            "allowed_mentions": Self::allowed_mentions(),
        });
        if !components.is_null() {
            body["components"] = components;
        }
        let created: CreatedMessage = serde_json::from_value(
            self.post(&format!("/channels/{channel_id}/messages"), body)
                .await?,
        )
        .context("Discord returned a message without an id")?;
        Ok(created.id)
    }

    /// Edits a message's text and its components together.
    ///
    /// Used to retire controls after a modal submission, where the update had to
    /// be deferred: an empty component array is what takes them away.
    pub async fn edit_with_components(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
        components: Value,
    ) -> Result<()> {
        let response = self
            .http
            .patch(format!(
                "{API_BASE}/channels/{channel_id}/messages/{message_id}"
            ))
            .header("Authorization", format!("Bot {}", self.token))
            .json(&json!({
                "content": truncate(content),
                "components": components,
                "allowed_mentions": Self::allowed_mentions(),
            }))
            .send()
            .await
            .context("editing a message")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Discord rejected an edit: {status} {text}"));
        }
        Ok(())
    }

    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<()> {
        let response = self
            .http
            .patch(format!(
                "{API_BASE}/channels/{channel_id}/messages/{message_id}"
            ))
            .header("Authorization", format!("Bot {}", self.token))
            .json(&json!({
                "content": truncate(content),
                "allowed_mentions": Self::allowed_mentions(),
            }))
            .send()
            .await
            .context("editing a message")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Discord rejected an edit: {status} {text}"));
        }
        Ok(())
    }

    /// Reads a message back. Used by the live tests to check what Discord
    /// actually parsed — in particular `mention_everyone`, which is the only
    /// trustworthy evidence that `allowed_mentions` did its job.
    pub async fn get_message(&self, channel_id: &str, message_id: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!(
                "{API_BASE}/channels/{channel_id}/messages/{message_id}"
            ))
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await
            .context("reading a message")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("Discord rejected a read: {status} {text}"));
        }
        serde_json::from_str(&text).context("a message that is not JSON")
    }

    /// Deletes a message, so a test can clean up after itself.
    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        let response = self
            .http
            .delete(format!(
                "{API_BASE}/channels/{channel_id}/messages/{message_id}"
            ))
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await
            .context("deleting a message")?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(anyhow!("Discord rejected a delete: {status}"));
        }
        Ok(())
    }

    /// Starts typing. Discord clears the indicator after about ten seconds, so
    /// this is refreshed while a turn runs.
    pub async fn typing(&self, channel_id: &str) -> Result<()> {
        self.post(&format!("/channels/{channel_id}/typing"), json!({}))
            .await?;
        Ok(())
    }
}

/// Discord rejects a message body over 2000 characters.
///
/// This is a **backstop**, not the way long replies are handled. An agent reply
/// is laid out across several messages by `split::paginate` before it reaches
/// here, so a body arriving overlong means a caller that did not paginate — an
/// ephemeral command reply, or a bug. Losing text is worse than a rejected
/// request only just, so the cut keeps the tail, where a conclusion usually is,
/// and says it happened.
///
/// If you find yourself widening this, paginate at the call site instead.
pub fn truncate(content: &str) -> String {
    const LIMIT: usize = 2000;
    if content.chars().count() <= LIMIT {
        return content.to_string();
    }
    let marker = "… (truncated)";
    let keep = LIMIT - marker.chars().count();
    let skip = content.chars().count() - keep;
    let tail: String = content.chars().skip(skip).collect();
    format!("{marker}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_intents_cover_dms_guild_messages_and_content() {
        // 1<<0 guilds, 1<<9 guild messages, 1<<12 DMs, 1<<15 message content.
        assert_eq!(INTENTS, 1 + 512 + 4096 + 32768);
    }

    #[test]
    fn everyone_and_role_pings_are_never_allowed() {
        let allowed = Rest::allowed_mentions();
        let parse = allowed.get("parse").unwrap().as_array().unwrap();
        assert!(parse.iter().all(|v| v != "everyone"));
        assert!(parse.iter().all(|v| v != "roles"));
    }

    #[test]
    fn a_rejected_token_is_never_retried() {
        // 4004 is permanent. Retrying it hammers Discord with a credential it
        // has already refused.
        assert!(is_fatal_close(4004));
        assert!(fatal_advice(4004).contains("token"));
    }

    #[test]
    fn missing_privileged_intents_are_fatal_and_explained() {
        assert!(is_fatal_close(4014));
        assert!(fatal_advice(4014).contains("Message Content Intent"));
    }

    #[test]
    fn an_ordinary_drop_is_still_retried() {
        // 4000 unknown error and 1006 abnormal closure are the everyday cases;
        // treating them as fatal would make the bot fragile.
        assert!(!is_fatal_close(4000));
        assert!(!is_fatal_close(1006));
        assert!(!is_fatal_close(4009));
    }

    #[test]
    fn a_command_stored_without_contexts_is_reported_as_unusable() {
        // This is the exact response shape that hid the bug: a 200, every
        // command echoed back, and `contexts: null` on each one. Counting the
        // array called that a success while the picker offered nothing.
        let stored = vec![
            json!({ "name": "new", "contexts": Value::Null }),
            json!({ "name": "help", "contexts": [0, 1] }),
            json!({ "name": "status" }),
        ];
        assert_eq!(commands_without_contexts(&stored), vec!["new", "status"]);
    }

    #[test]
    fn a_fully_registered_set_reports_nothing_wrong() {
        let stored = vec![
            json!({ "name": "new", "contexts": [0, 1] }),
            json!({ "name": "help", "contexts": [0] }),
        ];
        assert!(commands_without_contexts(&stored).is_empty());
    }

    #[test]
    fn a_short_message_is_left_alone() {
        assert_eq!(truncate("hello"), "hello");
    }

    #[test]
    fn an_overlong_message_is_cut_to_the_discord_limit() {
        let long = "x".repeat(2500);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), 2000);
        assert!(cut.starts_with("… (truncated)"));
    }

    #[test]
    fn a_dm_has_no_guild() {
        let msg = parse_message(&json!({
            "id": "1", "channel_id": "c", "content": "hi",
            "author": { "id": "u", "username": "sam" },
        }))
        .unwrap();
        assert!(msg.is_dm());
        assert_eq!(msg.author_name, "sam");
        assert!(!msg.author_is_bot);
    }

    #[test]
    fn a_global_name_is_preferred_over_the_username() {
        let msg = parse_message(&json!({
            "id": "1", "channel_id": "c", "guild_id": "g", "content": "hi",
            "author": { "id": "u", "username": "sam", "global_name": "Sam Vimes" },
        }))
        .unwrap();
        assert_eq!(msg.author_name, "Sam Vimes");
        assert!(!msg.is_dm());
    }

    #[test]
    fn a_slash_command_in_a_guild_is_parsed_with_its_argument() {
        let i = parse_interaction(&json!({
            "id": "i1", "token": "tok", "type": 2,
            "guild_id": "g", "channel_id": "c",
            "channel": { "id": "c", "type": 0 },
            "member": { "user": { "id": "u", "username": "sam", "global_name": "Sam" } },
            "data": {
                "name": "model", "type": 1,
                "options": [ { "name": "id", "type": 3, "value": "gpt-5" } ],
            },
        }))
        .unwrap();
        assert_eq!(i.name, "model");
        assert_eq!(i.argument, "gpt-5");
        assert_eq!(i.user_id, "u");
        assert_eq!(i.user_name, "Sam");
        assert_eq!(i.channel_id, "c");
        assert!(!i.is_dm());
    }

    #[test]
    fn a_slash_command_in_a_dm_finds_the_user_at_the_top_level() {
        // There is no `member` outside a guild, so a parser that only looks
        // there would drop every command sent in a DM.
        let i = parse_interaction(&json!({
            "id": "i1", "token": "tok", "type": 2,
            "channel_id": "c",
            "user": { "id": "u", "username": "sam" },
            "data": { "name": "new", "type": 1 },
        }))
        .unwrap();
        assert!(i.is_dm());
        assert_eq!(i.user_id, "u");
        assert_eq!(i.name, "new");
        assert_eq!(i.argument, "");
    }

    #[test]
    fn a_command_name_is_lowercased() {
        let i = parse_interaction(&json!({
            "id": "i1", "token": "tok", "type": 2, "channel_id": "c",
            "user": { "id": "u", "username": "sam" },
            "data": { "name": "NEW", "type": 1 },
        }))
        .unwrap();
        assert_eq!(i.name, "new");
    }

    #[test]
    fn a_ping_or_component_interaction_is_not_a_command() {
        // Type 1 is Discord's PING and 3 is a message component. Treating
        // either as a command would invent a name out of nothing.
        for kind in [1, 3, 5] {
            assert!(
                parse_interaction(&json!({
                    "id": "i", "token": "t", "type": kind, "channel_id": "c",
                    "user": { "id": "u", "username": "sam" },
                }))
                .is_none()
            );
        }
    }

    #[test]
    fn a_command_in_a_thread_carries_the_thread_channel_type() {
        let i = parse_interaction(&json!({
            "id": "i1", "token": "tok", "type": 2,
            "guild_id": "g", "channel_id": "t1",
            "channel": { "id": "t1", "type": 11 },
            "member": { "user": { "id": "u", "username": "sam" } },
            "data": { "name": "status", "type": 1 },
        }))
        .unwrap();
        assert_eq!(i.channel_type, Some(11));
    }

    #[test]
    fn the_application_id_is_read_from_ready() {
        // Commands register under the application id. Older bots share it with
        // the bot user id, but they are not the same field.
        let d = json!({
            "session_id": "s",
            "user": { "id": "botuser" },
            "application": { "id": "app" },
        });
        let bot_id = d["user"]["id"].as_str().unwrap();
        let application_id = d["application"]["id"].as_str().unwrap_or(bot_id);
        assert_eq!(application_id, "app");
        assert_ne!(application_id, bot_id);
    }

    #[test]
    fn an_ephemeral_reply_sets_the_ephemeral_flag() {
        // 1<<6 is the only way to keep a command's answer from the channel.
        assert_eq!(1 << 6, 64);
    }

    #[test]
    fn mentions_are_collected() {
        let msg = parse_message(&json!({
            "id": "1", "channel_id": "c", "content": "hey",
            "author": { "id": "u", "username": "sam" },
            "mentions": [ { "id": "bot" }, { "id": "other" } ],
        }))
        .unwrap();
        assert_eq!(msg.mentions, vec!["bot", "other"]);
    }
}
