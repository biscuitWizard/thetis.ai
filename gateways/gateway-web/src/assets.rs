//! The embedded single-page app.
//!
//! Every file the browser can fetch is listed in one table. Adding a stylesheet
//! or a new ES module is a single line here plus the file itself — there is no
//! build step and no bundler. A file missing from the table 404s at runtime with
//! the module graph half-loaded, which looks like a dead page rather than a
//! missing line.

pub struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub body: &'static str,
}

// Charset is spelled out on every one: a bare `text/javascript` lets the
// browser guess, and a guess of latin-1 turns every em dash in a comment into
// mojibake.
const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        mime: HTML,
        body: include_str!("ui/index.html"),
    },
    Asset {
        path: "/index.html",
        mime: HTML,
        body: include_str!("ui/index.html"),
    },
    // Design tokens live apart from layout so restyling is a one-file change.
    Asset {
        path: "/theme.css",
        mime: CSS,
        body: include_str!("ui/theme.css"),
    },
    Asset {
        path: "/app.css",
        mime: CSS,
        body: include_str!("ui/app.css"),
    },
    Asset {
        path: "/app.js",
        mime: JS,
        body: include_str!("ui/app.js"),
    },
    Asset {
        path: "/lib/dom.js",
        mime: JS,
        body: include_str!("ui/lib/dom.js"),
    },
    Asset {
        path: "/lib/socket.js",
        mime: JS,
        body: include_str!("ui/lib/socket.js"),
    },
    Asset {
        path: "/lib/store.js",
        mime: JS,
        body: include_str!("ui/lib/store.js"),
    },
    Asset {
        path: "/lib/markdown.js",
        mime: JS,
        body: include_str!("ui/lib/markdown.js"),
    },
    // Mermaid diagrams from a ```mermaid fence. Separate from markdown.js
    // because it is async and owns the vendored library's lifecycle.
    Asset {
        path: "/lib/mermaid.js",
        mime: JS,
        body: include_str!("ui/lib/mermaid.js"),
    },
    Asset {
        path: "/lib/toast.js",
        mime: JS,
        body: include_str!("ui/lib/toast.js"),
    },
    Asset {
        path: "/views/sessions.js",
        mime: JS,
        body: include_str!("ui/views/sessions.js"),
    },
    Asset {
        path: "/views/transcript.js",
        mime: JS,
        body: include_str!("ui/views/transcript.js"),
    },
    Asset {
        path: "/views/composer.js",
        mime: JS,
        body: include_str!("ui/views/composer.js"),
    },
    Asset {
        path: "/views/picker.js",
        mime: JS,
        body: include_str!("ui/views/picker.js"),
    },
    // @-mentions in the composer: the workspace index, the match menu, the
    // highlight, and turning mentioned paths into attachments.
    Asset {
        path: "/views/mentions.js",
        mime: JS,
        body: include_str!("ui/views/mentions.js"),
    },
    Asset {
        path: "/views/panel.js",
        mime: JS,
        body: include_str!("ui/views/panel.js"),
    },
    Asset {
        path: "/views/branch.js",
        mime: JS,
        body: include_str!("ui/views/branch.js"),
    },
    Asset {
        path: "/views/workspace.js",
        mime: JS,
        body: include_str!("ui/views/workspace.js"),
    },
    Asset {
        path: "/views/rail.js",
        mime: JS,
        body: include_str!("ui/views/rail.js"),
    },
    Asset {
        path: "/views/todo.js",
        mime: JS,
        body: include_str!("ui/views/todo.js"),
    },
    // Who else is in this conversation, and what their own account lets them do
    // here — which is the only place the per-speaker permission rule is visible.
    Asset {
        path: "/views/participants.js",
        mime: JS,
        body: include_str!("ui/views/participants.js"),
    },
    Asset {
        path: "/views/admin.js",
        mime: JS,
        body: include_str!("ui/views/admin.js"),
    },
    // The centre stage's tab strip: the conversation first and always, then a
    // tab per sub-agent and per open file. Owns which pane is showing, the
    // conversation's title and rename, and the file editors.
    Asset {
        path: "/views/stage.js",
        mime: JS,
        body: include_str!("ui/views/stage.js"),
    },
    Asset {
        path: "/views/context.js",
        mime: JS,
        body: include_str!("ui/views/context.js"),
    },
    // The portraits either side of the transcript, and the sidebar's avatar
    // button. Not a rail tab: an avatar is not something you inspect, it is
    // ambient identity, so it lives in the space beside the conversation.
    Asset {
        path: "/views/avatars.js",
        mime: JS,
        body: include_str!("ui/views/avatars.js"),
    },
    // The `ask_user` form. Lives in the transcript rather than the rail: the
    // questions are a message, and answering them is answering the agent.
    Asset {
        path: "/views/askuser.js",
        mime: JS,
        body: include_str!("ui/views/askuser.js"),
    },
    // The system status bar along the foot of the shell: trunk's version, the
    // build being served, the worker fleet, and the machine.
    Asset {
        path: "/views/statusbar.js",
        mime: JS,
        body: include_str!("ui/views/statusbar.js"),
    },
    // The terminal drawer: the shells the agent has open, live. Not a rail tab
    // — a terminal is watched *while* the agent works, so it docks along the
    // foot of the conversation and the transcript shortens to make room.
    Asset {
        path: "/views/terminal.js",
        mime: JS,
        body: include_str!("ui/views/terminal.js"),
    },
    // xterm.js 5.3.0, vendored under /vendor. views/terminal.js builds these
    // URLs relative to its own module URL rather than absolutely, so they
    // resolve under /preview/<session>/ as well as at the root.
    //
    // They remain the one exception to "hand-rolled, no
    // dependencies": a terminal emulator is a pile of correctness (wrapping,
    // scroll regions, 256-colour SGR, wide characters) that is not worth
    // reimplementing, and getting it wrong shows up as garbled build output.
    Asset {
        path: "/vendor/xterm.css",
        mime: CSS,
        body: include_str!("ui/vendor/xterm.css"),
    },
    Asset {
        path: "/vendor/xterm.js",
        mime: JS,
        body: include_str!("ui/vendor/xterm.js"),
    },
    Asset {
        path: "/vendor/xterm-addon-fit.js",
        mime: JS,
        body: include_str!("ui/vendor/xterm-addon-fit.js"),
    },
    // mermaid 11.17.2, vendored — the second exception to "no dependencies",
    // approved for the same reason as xterm: diagram layout is a pile of graph
    // algorithms not worth reimplementing. lib/mermaid.js builds this URL
    // relative to its own module URL so it resolves under /preview/<session>/,
    // and loads it only when a diagram actually appears — it is 3.5 MB, which
    // is most of this guest's size and must not be on the startup path.
    Asset {
        path: "/vendor/mermaid.js",
        mime: JS,
        body: include_str!("ui/vendor/mermaid.js"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every embedded script must actually parse.
    ///
    /// There is no build step and no bundler, so nothing between a text editor
    /// and the browser ever reads this JavaScript. `cargo build` compiles the
    /// Rust that embeds it and says nothing about its contents, and the guest's
    /// smoke test exercises the wire protocol rather than the page — so a
    /// syntax error here ships green and bricks the entire front end at
    /// runtime, with no error anywhere on the host.
    ///
    /// One did. A comment's first line gained a `*/`, closing the block early
    /// and leaving three lines of prose as code:
    ///
    /// ```text
    /// /* The websocket, with reconnect and login-expiry recovery. */
    ///  *
    ///  * Frames are handed to whoever registers for their `type` ...
    ///  */
    /// ```
    ///
    /// `lib/socket.js` failed to parse, so `app.js` never loaded, so nothing
    /// ran — and the page sat on the word "connecting", which is simply the
    /// static text in `index.html` that the script would have replaced. The
    /// whole UI was down, from one line of a comment, and the only symptom was
    /// a status that never changed.
    ///
    /// Skipped where node is absent rather than failed: this checks the
    /// assets, and a machine without a JavaScript engine has not made them
    /// wrong.
    #[test]
    fn every_embedded_script_parses() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: no node on PATH to parse with");
            return;
        }

        let dir = std::env::temp_dir().join(format!("thetis-assets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(dir.join("package.json"), r#"{"type":"module"}"#)
            .expect("marking scratch JavaScript as ES modules");

        let mut broken: Vec<String> = Vec::new();
        for asset in ASSETS.iter().filter(|a| a.mime == JS) {
            // Flattened, because the check only needs one file per asset and
            // the real paths would mean creating the tree.
            let file = dir.join(asset.path.replace('/', "_"));
            std::fs::write(&file, asset.body).expect("writing the asset out");
            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&file)
                .output()
                .expect("running node");
            if !out.status.success() {
                broken.push(format!(
                    "{}:\n{}",
                    asset.path,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            broken.is_empty(),
            "embedded JavaScript that does not parse — the browser would load none of it:\n\n{}",
            broken.join("\n\n")
        );
    }

    /// Every module the app imports has to be in the table.
    ///
    /// A file missing from `ASSETS` 404s, and a 404 on an ES module stops the
    /// whole graph loading — the same dead page, from the other direction. The
    /// table's own doc comment warns about this; the warning is worth a test.
    #[test]
    fn every_imported_module_is_served() {
        let mut missing: Vec<String> = Vec::new();
        for asset in ASSETS.iter().filter(|a| a.mime == JS) {
            let dir = asset.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            for spec in relative_imports(asset.body) {
                let resolved = resolve(dir, &spec);
                if find(&resolved).is_none() {
                    missing.push(format!("{} imports {spec} -> {resolved}", asset.path));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "imports with nothing behind them:\n  {}",
            missing.join("\n  ")
        );
    }

    /// The `./x.js` and `../x.js` specifiers in a module, ignoring bare and
    /// absolute ones. Deliberately crude — it only has to find the specifiers
    /// this codebase actually writes.
    fn relative_imports(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (idx, _) in body.match_indices("from \"") {
            let rest = &body[idx + 6..];
            if let Some(end) = rest.find('"') {
                let spec = &rest[..end];
                if spec.starts_with("./") || spec.starts_with("../") {
                    out.push(spec.to_string());
                }
            }
        }
        out
    }

    /// Joins a relative specifier onto the directory of the importing module.
    fn resolve(dir: &str, spec: &str) -> String {
        let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
        for segment in spec.split('/') {
            match segment {
                "." | "" => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        format!("/{}", parts.join("/"))
    }
}

/// Looks an asset up by request path. Linear over a table of a couple of dozen
/// entries, which costs less than hashing the string a map would have to hash.
/// A miss is a 404, so a new file added above is the difference between the
/// module graph loading and half of it 404ing at runtime.
pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.path == path)
}
