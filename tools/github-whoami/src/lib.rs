//! Verify the GitHub App credentials and report the identity they give.
//!
//! This is the diagnostic tool for the `github-*` group, and the first thing to
//! run after setting up the App. It exercises the whole authentication chain —
//! sign a JWT, call `/app`, list installations, mint an installation token,
//! resolve the bot user id — and reports which step failed rather than a bare
//! 401 from somewhere inside it.
//!
//! It is read-only: it changes nothing on GitHub.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod github;

use github::{account_of, Auth, GitHub};
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "github-whoami".to_string(),
            description: "Check the GitHub App credentials and show what identity they give: \
                          the app, its bot user, every account it is installed on, and the git \
                          author string to use for commits. Run this first when a github-* tool \
                          returns 401, 403 or 404, and after changing the App's permissions."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "verbose": {
                        "type": "boolean",
                        "description": "Also list the App's granted permissions and each \
                                        installation's repository selection. Defaults to false."
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json).unwrap_or(json!({}));
        let verbose = args
            .get("verbose")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let client = GitHub::from_config(&config_json)?;
        let mut out = String::new();

        if client.auth_kind() == Auth::Token {
            out.push_str(
                "Authenticating with a bare token, not a GitHub App.\n\n\
                 This works for reading and writing, but commits are authored by whoever owns \
                 the token rather than by a distinct [bot] identity, and /app endpoints are \
                 unavailable. Configure app_id + a private key for the better path.\n\n",
            );
            let user = client.get("/user", &[])?;
            let login = user
                .get("login")
                .and_then(Value::as_str)
                .unwrap_or("(unknown)");
            out.push_str(&format!("token belongs to: {login}\n"));
            return Ok(out);
        }

        // Step 1 and 2: sign a JWT and read the App's own record. A failure
        // here is a credential problem, before any installation is involved.
        let app = client.app_info().map_err(|e| {
            format!("could not authenticate as the App (step 1: JWT -> GET /app).\n\n{e}")
        })?;

        let name = app.get("name").and_then(Value::as_str).unwrap_or("(unnamed)");
        let slug = app.get("slug").and_then(Value::as_str).unwrap_or("");
        let owner = app
            .get("owner")
            .and_then(|o| o.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("(unknown)");

        out.push_str("GitHub App credentials are valid.\n\n");
        out.push_str(&format!("app:       {name}\n"));
        out.push_str(&format!("slug:      {slug}\n"));
        out.push_str(&format!("app id:    {}\n", client.app_id()));
        out.push_str(&format!("owner:     {owner}\n"));
        if let Some(url) = app.get("html_url").and_then(Value::as_str) {
            out.push_str(&format!("page:      {url}\n"));
        }

        // Step 3: the bot user id, which is what makes a commit attributable.
        match client.identity() {
            Ok(identity) => {
                out.push_str("\nCommit identity — set these on any clone before committing:\n");
                out.push_str(&format!("  git config user.name  \"{}\"\n", identity.name));
                out.push_str(&format!("  git config user.email \"{}\"\n", identity.email));
                out.push_str(&format!(
                    "\n(bot user id {} — GitHub links a commit to this App only when the \
                     committer email matches exactly.)\n",
                    identity.bot_user_id
                ));
            }
            Err(e) => out.push_str(&format!("\nCould not resolve the bot identity: {e}\n")),
        }

        // Step 4: installations. Without one, the App can reach no repository.
        let installations = client.list_installations()?;
        out.push_str(&format!("\ninstallations: {}\n", installations.len()));

        if installations.is_empty() {
            out.push_str(&format!(
                "\nThe App is not installed anywhere yet, so it cannot see any repository.\n{}\n",
                github::INSTALL_HELP
            ));
            return Ok(out);
        }

        for installation in &installations {
            let id = installation.get("id").and_then(Value::as_u64).unwrap_or(0);
            let selection = installation
                .get("repository_selection")
                .and_then(Value::as_str)
                .unwrap_or("?");
            out.push_str(&format!(
                "\n  {id}  {}  ({selection} repositories)\n",
                account_of(installation)
            ));

            if verbose {
                if let Some(permissions) = installation.get("permissions").and_then(Value::as_object)
                {
                    let mut keys: Vec<&String> = permissions.keys().collect();
                    keys.sort();
                    for key in keys {
                        let level = permissions[key].as_str().unwrap_or("?");
                        out.push_str(&format!("       {key}: {level}\n"));
                    }
                }
            }
        }

        if let Some(pinned) = client.configured_installation() {
            out.push_str(&format!("\nActing through installation {pinned} (pinned in config).\n"));
        } else if installations.len() == 1 {
            out.push_str("\nOne installation, so it is used automatically.\n");
        } else {
            out.push_str(
                "\nSeveral installations: set `installation_id` in [tools.github] to choose, \
                 or pass it per call where a tool accepts it.\n",
            );
        }

        // Step 5: actually mint a token. Everything above can pass while this
        // fails, so proving it works is the point of the tool.
        match client.installation_token() {
            Ok(_) => out.push_str("\nInstallation token minted successfully — the full chain works.\n"),
            Err(e) => out.push_str(&format!("\nCould not mint an installation token: {e}\n")),
        }

        // The repositories reachable right now is the most useful single fact
        // for planning work, and it needs the token that was just proven.
        match client.get("/installation/repositories", &[("per_page".into(), "100".into())]) {
            Ok(response) => {
                let repos = response
                    .get("repositories")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                out.push_str(&format!("\nrepositories reachable: {}\n", repos.len()));
                for repo in repos.iter().take(40) {
                    let full = repo
                        .get("full_name")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    let private = repo
                        .get("private")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    out.push_str(&format!(
                        "  {full}{}\n",
                        if private { " (private)" } else { "" }
                    ));
                }
                if repos.len() > 40 {
                    out.push_str(&format!("  ... and {} more\n", repos.len() - 40));
                }
            }
            Err(e) => out.push_str(&format!("\nCould not list repositories: {e}\n")),
        }

        Ok(out)
    }
}

export!(Component);
