//! What to do with a tool result that is too big for the context window.
//!
//! The old answer was to cut it at 32 KiB, keep the head, and say
//! `[truncated: N of M bytes shown]`. That is a dead end in the most literal
//! sense: the model is told the answer was clipped and given no way to reach the
//! rest of it. Worse, keeping the head threw away the *end* of the text — which
//! is exactly where a well-behaved tool puts its own resumption footer, so
//! `read_path`'s "read on with offset 240" was reliably destroyed by the very
//! mechanism that made reading on necessary.
//!
//! So oversized output is spilled to a file instead. The model gets the head, the
//! tail, and the path — and the path is reachable with `read_path` and
//! `search_files`, which already know how to window and grep a file. That is the
//! whole design: rather than invent a pagination protocol per tool, hand back
//! something the existing file tools can chew on.
//!
//! Nothing is lost. The bytes are on disk, in the shared workspace, where both
//! the agent and the operator can see them.

use crate::config::Config;
use std::path::PathBuf;

/// Where spilled output lands, under the shared workspace.
const SPILL_DIR: &str = "tool-output";

/// How much of the head to keep when spilling. The opening of a result usually
/// says what kind of thing it is.
const HEAD_FRACTION: usize = 4;

/// How much of the tail to keep. A resumption footer, a row count, an error
/// summary and a stack trace's actual cause all live at the end, which is the
/// half the old truncation always discarded.
const TAIL_FRACTION: usize = 8;

/// Steps back to the nearest character boundary at or below `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Steps forward to the nearest character boundary at or above `at`.
fn ceil_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Prefers a line break near a cut, so a spilled excerpt does not end
/// mid-token. Only accepts one reasonably close to the target, because a cut in
/// the right place matters more than a cut on a tidy boundary.
fn nearest_newline(text: &str, at: usize, window: usize) -> usize {
    // Both ends of the slice must sit on character boundaries, not just `at`:
    // `at - window` lands mid-character on any multi-byte text and slicing there
    // panics. That is a crash in the kernel's output path, reachable by any tool
    // returning CJK, emoji or accented prose.
    let at = floor_boundary(text, at);
    let lo = floor_boundary(text, at.saturating_sub(window));
    if let Some(found) = text[lo..at].rfind('\n') {
        return lo + found + 1;
    }
    at
}

/// A spill that happened, or the reason one could not.
pub struct Spilled {
    /// Guest-facing path, e.g. `/workspace/tool-output/....txt`.
    pub path: Option<String>,
    /// The text to hand to the model in place of the original.
    pub text: String,
}

/// Writes `text` somewhere the file tools can reach and returns its guest path.
fn write_spill(cfg: &Config, label: &str, text: &str) -> anyhow::Result<String> {
    let root: &PathBuf = cfg
        .wasi
        .dirs
        .first()
        .ok_or_else(|| anyhow::anyhow!("no workspace directory is configured"))?;
    let dir = root.join(SPILL_DIR);
    std::fs::create_dir_all(&dir)?;

    // Name it after the tool and the time, so a directory listing is readable
    // and two spills in the same second do not collide.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}-{stamp}.txt"));
    std::fs::write(&path, text.as_bytes())?;
    Ok(crate::hostfs::display(cfg, &path))
}

/// Caps `text` for the context window, spilling the whole of it to a file when
/// it does not fit.
///
/// `label` names the source — a tool name, or something like `read_path` — and
/// is used for the spill filename.
pub fn cap(cfg: &Config, label: &str, text: String) -> Spilled {
    let limit = cfg.max_tool_output_bytes;
    if text.len() <= limit {
        return Spilled {
            path: None,
            text,
        };
    }

    let total = text.len();

    // Try to put the bytes on disk first: what we say to the model depends on
    // whether that worked, and promising a path that does not exist is worse
    // than admitting the text is gone.
    let spilled = write_spill(cfg, label, &text);

    // Reserve room for the notice itself, so the result still fits the budget
    // once the explanation is appended.
    let notice_budget = 400;
    let body = limit.saturating_sub(notice_budget).max(limit / 2);

    let head_len = floor_boundary(&text, body / HEAD_FRACTION * (HEAD_FRACTION - 1));
    let tail_len = body / TAIL_FRACTION;

    let head_end = nearest_newline(&text, head_len, 200);
    let tail_start = ceil_boundary(&text, total.saturating_sub(tail_len));
    // Keep the tail whole-line too, but never let it swallow the head.
    let tail_start = if tail_start > head_end {
        match text[tail_start..].find('\n') {
            // +1 is safe: a newline is one byte and ASCII, so just past it is a
            // character boundary.
            Some(offset) if offset < 200 => tail_start + offset + 1,
            _ => tail_start,
        }
    } else {
        head_end
    };

    let head = &text[..head_end];
    let tail = &text[tail_start..];
    let dropped = tail_start.saturating_sub(head_end);

    let mut out = String::with_capacity(limit);
    out.push_str(head);
    out.push_str("\n\n");

    match &spilled {
        Ok(path) => {
            out.push_str(&format!(
                "[... {dropped} of {total} bytes not shown here ...]\n\n"
            ));
            if !tail.is_empty() {
                out.push_str("--- the end of the output ---\n");
                out.push_str(tail);
                if !tail.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            // The recovery instruction goes last, and is the whole point: this
            // used to be the part that got cut off.
            out.push_str(&format!(
                "[This output was {total} bytes, over the {limit}-byte limit, so all of it \
                 was written to {path}. Do not re-run the command: read that file instead. \
                 `read_path` with `offset`/`limit` windows it, and `search_files` with \
                 `path` set to it finds a specific line.]"
            ));
        }
        Err(e) => {
            out.push_str(&format!(
                "[... {dropped} of {total} bytes not shown here ...]\n\n"
            ));
            if !tail.is_empty() {
                out.push_str("--- the end of the output ---\n");
                out.push_str(tail);
                if !tail.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(&format!(
                "[This output was {total} bytes, over the {limit}-byte limit. Saving the \
                 rest to a file failed ({e}), so the middle is genuinely lost — narrow the \
                 request rather than repeating it.]"
            ));
        }
    }

    Spilled {
        path: spilled.ok(),
        text: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical");
        let ws = root.join("workspace");
        std::fs::create_dir_all(&ws).expect("workspace");

        let mut cfg = Config::load().expect("config");
        cfg.root = root.clone();
        cfg.wasi.dirs = vec![ws];
        cfg.filesystem.roots = vec![root];
        cfg.filesystem.enabled = true;
        cfg.max_tool_output_bytes = 2_000;
        (cfg, dir)
    }

    #[test]
    fn output_within_the_budget_is_untouched() {
        let (cfg, _d) = fixture();
        let out = cap(&cfg, "read_path", "small".to_string());
        assert_eq!(out.text, "small");
        assert!(out.path.is_none(), "nothing to spill");
    }

    /// The regression that motivated all of this: a footer telling the model how
    /// to continue used to sit exactly where the cut landed.
    #[test]
    fn a_resumption_footer_at_the_end_survives() {
        let (cfg, _d) = fixture();
        let mut text = "x\n".repeat(5_000);
        text.push_str("[lines 1-100 of 900; read on with offset 101]");

        let out = cap(&cfg, "read_path", text);
        assert!(
            out.text.contains("read on with offset 101"),
            "the tail carries the recovery instruction: {}",
            out.text
        );
    }

    #[test]
    fn the_whole_output_is_written_where_the_file_tools_can_reach_it() {
        let (cfg, _d) = fixture();
        let text = "line\n".repeat(5_000);
        let out = cap(&cfg, "terminal_run", text.clone());

        let path = out.path.expect("should have spilled");
        assert!(
            path.starts_with("/workspace/tool-output/"),
            "guest-facing path: {path}"
        );
        // The guest spelling must resolve back to the real file.
        let resolved = crate::hostfs::resolve(&cfg, &path).expect("resolves");
        let written = std::fs::read_to_string(resolved).expect("readable");
        assert_eq!(written, text, "every byte is kept");
    }

    #[test]
    fn the_result_stays_within_the_budget() {
        let (cfg, _d) = fixture();
        let out = cap(&cfg, "tool", "y".repeat(500_000));
        assert!(
            out.text.len() <= cfg.max_tool_output_bytes,
            "capped output must fit the budget, got {}",
            out.text.len()
        );
    }

    #[test]
    fn it_says_what_to_do_next_rather_than_only_what_happened() {
        let (cfg, _d) = fixture();
        let out = cap(&cfg, "tool", "z".repeat(50_000));
        assert!(out.text.contains("read_path"), "names the way to read on");
        assert!(out.text.contains("search_files"), "and the way to search");
        assert!(
            out.text.contains("Do not re-run"),
            "a re-run would just spill again"
        );
    }

    /// Multi-byte characters must not be sliced in half at any cut. Slicing a
    /// `&str` off a boundary panics, so this is a kernel crash reachable by any
    /// tool that returns CJK, emoji or accented prose — not a cosmetic concern.
    #[test]
    fn cuts_land_on_character_boundaries() {
        let (cfg, _d) = fixture();
        for body in [
            "日本語テキスト".repeat(5_000),
            "café ".repeat(20_000),
            "🙂🙃".repeat(20_000),
            // No newlines at all: the line-seeking paths must still hold.
            "あ".repeat(50_000),
        ] {
            let out = cap(&cfg, "tool", body.clone());
            assert!(out.path.is_some(), "large input should spill");
            // The spilled file must be byte-identical, and the returned excerpt
            // must be valid UTF-8 whose pieces really came from the input.
            let resolved =
                crate::hostfs::resolve(&cfg, &out.path.unwrap()).expect("resolves");
            assert_eq!(
                std::fs::read(resolved).expect("readable"),
                body.as_bytes(),
                "every byte preserved"
            );
            assert!(std::str::from_utf8(out.text.as_bytes()).is_ok());
        }
    }
}
