//! List the users in a Notion workspace, or identify the current connection.
//!
//! `GET /v1/users` and `GET /v1/users/me`. Small, but it closes a real gap: a
//! `people` property is written with user ids, and there is no other way to turn
//! "assign this to Priya" into an id the API will accept.
//!
//! `GET /v1/users/me` also answers "is my token working at all", which is the
//! first thing worth checking when everything returns 404 — a token can be
//! perfectly valid and still see nothing, because Notion shares no pages with a
//! connection until someone adds it to them.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 100;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-users".to_string(),
            description: "List the people in a Notion workspace with their user ids — needed to \
                          set a 'people' property — or, with whoami, show which connection the \
                          configured token belongs to. Use whoami to check the token works when \
                          other calls return 404."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "whoami": {
                        "type": "boolean",
                        "description": "Show the connection this token authenticates as, instead \
                                        of listing users. Good for verifying the token."
                    },
                    "query": {
                        "type": "string",
                        "description": "Only users whose name or email contains this text, \
                                        matched here rather than by the API."
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!("Maximum users to gather, 1-{MAX_LIMIT}. \
                                                Defaults to {DEFAULT_LIMIT}.")
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;

        if args.get("whoami").and_then(Value::as_bool) == Some(true) {
            let me = client.get("/v1/users/me", &[])?;
            return Ok(render_me(&me));
        }

        let want = notion::limit(&args, DEFAULT_LIMIT, MAX_LIMIT);
        let (users, next) = client.paginate("GET", "/v1/users", &json!({}), want)?;

        // Notion has no user search parameter, so filtering happens here. Worth
        // having anyway: a large workspace's user list is mostly noise.
        let query = notion::optional_str(&args, "query").map(|q| q.to_lowercase());
        let matched: Vec<&Value> = users
            .iter()
            .filter(|user| match &query {
                None => true,
                Some(query) => {
                    let name = user
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_lowercase();
                    let email = email_of(user).unwrap_or_default().to_lowercase();
                    name.contains(query) || email.contains(query)
                }
            })
            .collect();

        if matched.is_empty() {
            return Ok(match &query {
                Some(query) => format!(
                    "No users match {query:?} among the {} the connection can see.",
                    users.len()
                ),
                None => "This connection can see no users. Listing users needs the \
                         'read user information' capability, which is set per connection."
                    .to_string(),
            });
        }

        let mut out = format!("{} user(s).\n", matched.len());
        for user in &matched {
            let kind = user.get("type").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!(
                "\n- {} [{}]\n  id: {}\n",
                user.get("name").and_then(Value::as_str).unwrap_or("(no name)"),
                if kind.is_empty() { "user" } else { kind },
                user.get("id").and_then(Value::as_str).unwrap_or("?")
            ));
            if let Some(email) = email_of(user) {
                out.push_str(&format!("  email: {email}\n"));
            }
        }

        out.push_str("\nWrite a people property with these ids, e.g. {\"Owner\": [\"<id>\"]}.\n");
        if query.is_none() {
            out.push_str(&notion::pagination_note(matched.len(), next.as_ref()));
        }
        Ok(out)
    }
}

fn email_of(user: &Value) -> Option<String> {
    user.get("person")
        .and_then(|p| p.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn render_me(me: &Value) -> String {
    let mut out = String::from("This token authenticates as:\n");
    out.push_str(&format!(
        "  name: {}\n",
        me.get("name").and_then(Value::as_str).unwrap_or("(none)")
    ));
    out.push_str(&format!(
        "  id: {}\n",
        me.get("id").and_then(Value::as_str).unwrap_or("?")
    ));
    let kind = me.get("type").and_then(Value::as_str).unwrap_or("");
    out.push_str(&format!("  type: {kind}\n"));

    if let Some(bot) = me.get("bot") {
        if let Some(owner) = bot.get("owner") {
            let owner_type = owner.get("type").and_then(Value::as_str).unwrap_or("?");
            out.push_str(&format!("  owner: {owner_type}\n"));
            if let Some(name) = owner
                .get("user")
                .and_then(|u| u.get("name"))
                .and_then(Value::as_str)
            {
                out.push_str(&format!("  acting for: {name}\n"));
            }
        }
        if let Some(workspace) = bot.get("workspace_name").and_then(Value::as_str) {
            out.push_str(&format!("  workspace: {workspace}\n"));
        }
    }

    out.push_str(
        "\nThe token works. Note that authenticating says nothing about what it can see: Notion \
         shares no content with a connection until a page or database is explicitly connected to \
         it. If reads return 404, sharing is the thing to check.\n",
    );
    out
}

export!(Component);
