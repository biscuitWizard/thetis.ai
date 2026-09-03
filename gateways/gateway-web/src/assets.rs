//! The embedded single-page app.
//!
//! Every file the browser can fetch is listed in one table. Adding a stylesheet
//! or a new ES module is a single line here plus the file itself — there is no
//! build step and no bundler.

pub struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub body: &'static str,
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

pub const ASSETS: &[Asset] = &[
    Asset { path: "/", mime: HTML, body: include_str!("ui/index.html") },
    Asset { path: "/index.html", mime: HTML, body: include_str!("ui/index.html") },
    // Design tokens live apart from layout so restyling is a one-file change.
    Asset { path: "/theme.css", mime: CSS, body: include_str!("ui/theme.css") },
    Asset { path: "/app.css", mime: CSS, body: include_str!("ui/app.css") },
    Asset { path: "/app.js", mime: JS, body: include_str!("ui/app.js") },
    Asset { path: "/lib/dom.js", mime: JS, body: include_str!("ui/lib/dom.js") },
    Asset { path: "/lib/socket.js", mime: JS, body: include_str!("ui/lib/socket.js") },
    Asset { path: "/lib/store.js", mime: JS, body: include_str!("ui/lib/store.js") },
    Asset { path: "/lib/markdown.js", mime: JS, body: include_str!("ui/lib/markdown.js") },
    Asset { path: "/lib/toast.js", mime: JS, body: include_str!("ui/lib/toast.js") },
    Asset { path: "/views/sessions.js", mime: JS, body: include_str!("ui/views/sessions.js") },
    Asset { path: "/views/transcript.js", mime: JS, body: include_str!("ui/views/transcript.js") },
    Asset { path: "/views/composer.js", mime: JS, body: include_str!("ui/views/composer.js") },
    Asset { path: "/views/picker.js", mime: JS, body: include_str!("ui/views/picker.js") },
    Asset { path: "/views/panel.js", mime: JS, body: include_str!("ui/views/panel.js") },
    Asset { path: "/views/branch.js", mime: JS, body: include_str!("ui/views/branch.js") },
    Asset { path: "/views/workspace.js", mime: JS, body: include_str!("ui/views/workspace.js") },
    Asset { path: "/views/rail.js", mime: JS, body: include_str!("ui/views/rail.js") },
    Asset { path: "/views/context.js", mime: JS, body: include_str!("ui/views/context.js") },
    // The `ask_user` form. Lives in the transcript rather than the rail: the
    // questions are a message, and answering them is answering the agent.
    Asset { path: "/views/askuser.js", mime: JS, body: include_str!("ui/views/askuser.js") },
    // The system status bar along the foot of the shell: trunk's version, the
    // build being served, the worker fleet, and the machine.
    Asset { path: "/views/statusbar.js", mime: JS, body: include_str!("ui/views/statusbar.js") },
];

pub fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.path == path)
}
