//! The shared workspace, as a paragraph of the system prompt.
//!
//! `/workspace` is the one directory a WASI guest is handed as a preopen, and it
//! is shared by every conversation and every branch — the opposite of the
//! per-conversation checkout everything else operates on. That makes it the only
//! place where work outlives a conversation, and it was invisible: nothing in
//! the prompt said it existed, so a turn that should have put a clone or a data
//! file there put it in the branch instead, or spent tool calls rediscovering
//! the directory from scratch.
//!
//! So the prompt carries two things: what the workspace is *for*, and one level
//! of its contents. One level on purpose. It is what answers "what do I have
//! access to", it is a handful of lines, and it is stable — the top level of a
//! shared directory changes when a project is added, not when a build runs — so
//! the prompt stays byte-identical between turns and the provider's cache keeps
//! hitting. A deep tree would have neither property.

/// The preopen, as every guest sees it.
const GUEST_ROOT: &str = "/workspace";

/// Entries listed before the rest are summarised as a count. Far more than a
/// real workspace holds at its top level; the cap exists so a pathological
/// directory cannot eat the prompt.
const MAX_ENTRIES: usize = 120;

/// The `# Shared workspace` section, or `None` when there is no preopen to
/// describe.
///
/// `host_path` is `workspace_dir` from configuration: where the directory
/// actually is on this machine. Worth stating because a terminal command has to
/// name it that way — the host file tools are rooted at the conversation's own
/// checkout, and the workspace sits outside it.
pub fn section(host_path: Option<&str>) -> Option<String> {
    let listing = list()?;

    let mut out = String::from("\n# Shared workspace\n");
    out.push_str(
        "`/workspace` is a real directory, preopened for you and for every tool, and shared by \
         every conversation and every branch. Your checkout is yours alone and is for changing \
         Thetis itself; the workspace is for everything else — cloned repositories, build trees, \
         data, generated artifacts, anything meant to outlive this conversation or to be found by \
         another agent or by the operator, who browses the same tree in the UI. Nothing there is \
         private, so namespace what you create and do not assume you are the only writer.\n",
    );

    if let Some(host_path) = host_path.map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str(&format!(
            "\nYou and your tools reach it as `{GUEST_ROOT}`. On the host it is `{host_path}`, \
             which is the spelling a terminal command needs: the file tools are rooted at your \
             checkout and cannot see it.\n"
        ));
    }

    out.push_str(&format!("\nTop level of `{GUEST_ROOT}`:\n"));
    if listing.is_empty() {
        out.push_str("\n- (empty)\n");
        return Some(out);
    }
    for line in listing.iter().take(MAX_ENTRIES) {
        out.push_str(&format!("\n- {line}"));
    }
    if listing.len() > MAX_ENTRIES {
        out.push_str(&format!(
            "\n- … and {} more",
            listing.len() - MAX_ENTRIES
        ));
    }
    out.push('\n');
    Some(out)
}

/// One level of the preopen, directories first and then alphabetically — the
/// order a reader expects, and a fixed one, which is what keeps the prompt
/// stable across turns.
///
/// `None` means the preopen is not there at all (`wasi.dirs` empty, or the grant
/// withdrawn), which is the one case where saying nothing is better than saying
/// something: a section describing a directory the model cannot open would be
/// worse than no section.
fn list() -> Option<Vec<String>> {
    let entries = std::fs::read_dir(GUEST_ROOT).ok()?;

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or_else(|_| entry.path().is_dir());
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            // Sizes are deliberately left out. They are of little use at this
            // level and they change on every edit, which would cost a prompt
            // cache miss for no gain.
            files.push(name);
        }
    }

    dirs.sort_by_key(|n| n.to_lowercase());
    files.sort_by_key(|n| n.to_lowercase());
    dirs.extend(files);
    Some(dirs)
}
