wit_bindgen::generate!({world:"tool",path:"../../wit",generate_all});
mod moo;
use moo::{bounded, confined_objdef_path};
use serde_json::{json, Value};

struct Component;

/// Lines returned when the caller does not say. Chosen so a default read of a
/// typical objdef comes back whole, while a large one stops with a usable
/// continuation hint rather than being guillotined by the host.
const DEFAULT_LIMIT: u32 = 400;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "moo-read-objdef-file".into(),
            description: "Read UTF-8 objdef text beneath workspace/torchship-objdef, with line numbers. \
Reads a window of lines, not the whole file: pass `offset` and `limit` to page through a large \
one, and the footer says which offset to read on from. Traversal and symlink escapes are rejected."
                .into(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path beneath workspace/torchship-objdef."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "First line to read, counting from 1. Omit to start at the top.",
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many lines to read. Defaults to 400.",
                        "minimum": 1
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Only return lines containing this substring, with their line numbers. \
Use this to find a verb or property in a large objdef instead of paging through it."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["group:moo".into(), "read-only".into()],
        }
    }

    fn invoke(_: String, a: String, _: String) -> Result<String, String> {
        let a: Value = serde_json::from_str(&a).map_err(|e| e.to_string())?;
        let p = confined_objdef_path(a.get("path").and_then(Value::as_str).ok_or("missing path")?)?;
        let s = std::fs::read_to_string(p).map_err(|e| e.to_string())?;

        let lines: Vec<&str> = s.lines().collect();
        let total = lines.len();

        // A search is a different question from a read, and answering it by
        // returning the file and letting the caller scan is what made this tool
        // expensive. Matching here returns tens of lines instead of thousands.
        if let Some(pat) = a.get("pattern").and_then(Value::as_str) {
            let hits: Vec<String> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains(pat))
                .map(|(i, l)| format!("{:>6}\t{}", i + 1, l))
                .collect();
            if hits.is_empty() {
                return Ok(format!("no line contains {pat:?} in {total} lines"));
            }
            let body = hits.join("\n");
            return Ok(bounded(
                &format!("{} line(s) of {total} contain {pat:?}:\n{body}", hits.len()),
                32000,
            ));
        }

        let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
        let limit = a
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_LIMIT as u64)
            .max(1) as usize;

        if offset > total {
            return Ok(format!(
                "offset {offset} is past the end; the file has {total} lines"
            ));
        }

        let want_end = offset.saturating_add(limit).saturating_sub(1).min(total);

        // Bound by bytes as well as by lines. A line limit alone is the unit
        // mismatch that made this class of bug: on a file with long lines the
        // tool believes it returned everything, writes no continuation hint, and
        // the host's cut then removes the tail where the hint would have been.
        // Stopping here instead means the footer below is always the truth.
        const BYTE_BUDGET: usize = 28_000;
        let mut body = String::new();
        let mut end = offset - 1;
        let mut stopped_early = false;
        for (i, l) in lines[offset - 1..want_end].iter().enumerate() {
            let row = format!("{:>6}\t{}\n", offset + i, l);
            if !body.is_empty() && body.len() + row.len() > BYTE_BUDGET {
                stopped_early = true;
                break;
            }
            body.push_str(&row);
            end = offset + i;
        }

        // The footer is the whole point of the change: it goes last, it says
        // where it stopped, and it names the offset that continues from here.
        let footer = if stopped_early {
            format!(
                "[lines {offset}-{end} of {total} (stopped at the output size limit); \
read on with offset {}]",
                end + 1
            )
        } else if end < total {
            format!("[lines {offset}-{end} of {total}; read on with offset {}]", end + 1)
        } else if offset > 1 {
            format!("[lines {offset}-{end} of {total}; end of file]")
        } else {
            format!("[{total} lines, complete]")
        };

        Ok(format!("{body}\n{footer}"))
    }
}

export!(Component);
