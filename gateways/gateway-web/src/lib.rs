//! The Thetis web chat gateway.
//!
//! Owns the whole user-facing surface: the single-page app and the JSON wire
//! protocol spoken over the websocket. The orchestrator owns only the socket,
//! so everything here — layout, styling, protocol — can be rewritten and
//! hot-swapped while people are connected.
//!
//! The pieces:
//!   `assets`   the embedded app, one table entry per file
//!   `handlers` one function per client action
//!   `render`   session events to wire frames

wit_bindgen::generate!({
    world: "gateway",
    path: "../../wit",
    generate_all,
    additional_derives: [serde::Serialize],
});

mod assets;
mod handlers;
mod render;

use serde_json::Value;

use crate::thetis::grip::sys;

struct Component;

/// Replaced in served assets with the configured agent name.
///
/// The distinction this keeps: *Thetis* is the harness — the process, the
/// branch machinery, the version in the status bar — while the agent that
/// talks to you is named by `agent.name` and defaults to Thetis too. Only
/// text that refers to the agent carries this placeholder.
const AGENT_NAME_PLACEHOLDER: &str = "{agent_name}";

/// The name used when config does not supply one.
///
/// The agent defaults to sharing the harness's name, so an unconfigured system
/// still reads coherently.
const DEFAULT_AGENT_NAME: &str = "Thetis";


/// Placeholders `fill_identity` substitutes, for reference when editing the
/// markup: `{agent_name}`, `{agent_avatar}`, `{agent_favicon}`,
/// `{agent_avatar_hidden}`, `{agent_mark_hidden}`. Only `text/html` assets are
/// substituted, so a placeholder in CSS or JS would be served verbatim — JS
/// reads the name from `data-agent-name` via `AGENT_NAME` in `lib/dom.js`.

/// The built-in tab icon, used when no avatar is configured.
///
/// Single quotes throughout so it can sit inside a double-quoted `href`, and
/// `%23` for the `#` that would otherwise be read as a fragment. It matches the
/// `<svg>` brand mark, but cannot use `currentColor` — a favicon has no
/// inherited colour — so the accent is written out.
const BUILT_IN_ICON: &str = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' \
viewBox='0 0 32 32'><circle cx='16' cy='16' r='9' fill='none' stroke='%237c9cff' \
stroke-width='2.5'/><circle cx='16' cy='16' r='3' fill='%237c9cff'/></svg>";

/// Escapes text for HTML, in both element and double-quoted attribute contexts.
///
/// The name reaches the document as an attribute value as well as visible text,
/// so a `"` or a `<` in it would otherwise rewrite the markup around it. A name
/// is configuration rather than user input, so this guards against a surprising
/// character — an ampersand in "Ada & Co" — more than against an attacker.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Fills in the agent's identity in an HTML asset: its name in the brand and
/// the window title, and its avatar in the sidebar and the tab icon.
///
/// Everything the agent is named in goes through here, so a rename reaches all
/// of it at once. The harness's own identity — the version and WIT contract in
/// the status bar — is deliberately left alone.
///
/// The sidebar avatar is expressed as two placeholders rather than one because
/// the markup holds both an `<img>` and the built-in `<svg>` mark, and exactly
/// one is shown. Toggling a `hidden` attribute is the house rule for hiding,
/// and it keeps the fallback available to the client: if the image 404s,
/// `app.js` can swap back to the mark without asking the host for new markup.
/// The favicon cannot work that way — there is no load event to recover from —
/// so it is substituted whole.
fn fill_identity(body: &str) -> String {
    let name = sys::config_get("agent_name")
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string());

    // Empty is a real choice here, not a missing value: it selects the built-in
    // mark rather than meaning "unset, go and find a default".
    let avatar = sys::config_get("agent_avatar").unwrap_or_default();
    let avatar = avatar.trim();
    let has_avatar = !avatar.is_empty();

    // A quote in a configured URL would close the attribute it sits in and let
    // the rest be read as markup, so it is escaped everywhere the URL lands.
    let avatar_attr = avatar.replace('"', "%22");

    // The tab icon follows the avatar. Substituted whole, rather than just the
    // URL, so the empty case cannot leave `href=""` — that resolves to the page
    // itself and makes the browser show a broken icon.
    let favicon = if has_avatar {
        avatar_attr.as_str()
    } else {
        BUILT_IN_ICON
    };

    body.replace(AGENT_NAME_PLACEHOLDER, &escape_html(name.trim()))
        .replace("{agent_avatar}", &avatar_attr)
        .replace("{agent_favicon}", favicon)
        .replace(
            "{agent_avatar_hidden}",
            if has_avatar { "" } else { "hidden" },
        )
        .replace("{agent_mark_hidden}", if has_avatar { "hidden" } else { "" })
}

impl Guest for Component {
    fn describe() -> GatewayManifest {
        GatewayManifest {
            name: "web".to_string(),
            version_note: "multi-session chat: streaming, attachments, mode and model pickers"
                .to_string(),
        }
    }

    fn serve_asset(path: String) -> Option<Asset> {
        assets::find(&path).map(|a| {
            // Only HTML carries the placeholder today, so the substitution is
            // limited to it rather than run over every stylesheet and the
            // vendored terminal emulator.
            let body = if a.mime.starts_with("text/html") {
                fill_identity(a.body)
            } else {
                a.body.to_string()
            };
            Asset {
                mime: a.mime.to_string(),
                bytes: body.into_bytes(),
            }
        })
    }

    fn on_client_message(_client_id: String, frame_json: String) -> Vec<GatewayAction> {
        match serde_json::from_str::<Value>(&frame_json) {
            Ok(frame) => handlers::dispatch(&frame),
            Err(e) => vec![handlers::error(format!("malformed frame: {e}"))],
        }
    }

    fn render_event(event: OutboundEvent) -> Option<String> {
        render::event(&event).map(|v| v.to_string())
    }
}

export!(Component);
