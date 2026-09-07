//! Session events to wire frames.
//!
//! Sub-agent frames pass through here unchanged: a child's events render
//! exactly as a parent's do, and the `agent` / `agent_label` / `agent_parent`
//! tags that tell the transcript whose they are are added outside this module —
//! natively in the worker for a live frame, and in `handlers::history` for a
//! replay. Neither of those has anything this renderer needs to know about.
//!
//! Written out explicitly rather than derived from the WIT variant, so the
//! protocol the browser sees stays readable and can evolve independently of the
//! event schema. Each arm produces the `kind` the transcript view switches on.

use crate::thetis::grip::types::{OutboundEvent, SessionEvent};
use serde_json::{json, Value};

pub fn event(ev: &OutboundEvent) -> Option<Value> {
    let mut body = match &ev.event {
        SessionEvent::UserMessage(msg) => json!({
            "kind": "user",
            "text": msg.text,
            // Absent for every message stored before authorship, and for
            // anything the system wrote to the transcript itself. The view must
            // treat absence as "the reader", which is what it always assumed.
            "author": msg.author.as_ref().map(|a| json!({
                "id": a.id,
                "display": a.display,
                "surface": a.surface,
            })),
            // Only what the UI needs to draw a thumbnail; the bytes are already
            // in the browser that sent them, and other tabs re-fetch on demand.
            "attachments": msg.attachments.iter().map(|a| json!({
                "name": a.name,
                "mime": a.mime,
                "data": if a.mime.starts_with("image/") {
                    Value::String(format!("data:{};base64,{}", a.mime, a.data_base64))
                } else {
                    Value::Null
                },
            })).collect::<Vec<_>>(),
        }),

        SessionEvent::AssistantMessage(msg) => json!({
            "kind": "assistant",
            "text": msg.content,
            "model": msg.model,
            "usage": msg.usage.as_ref().map(|u| json!({
                "prompt": u.prompt_tokens,
                "completion": u.completion_tokens,
                "cost": u.cost_usd,
                "cached": u.cached_tokens,
                "cache_write": u.cache_write_tokens,
            })),
        }),

        SessionEvent::StreamDelta(chunk) => json!({ "kind": "delta", "text": chunk }),

        SessionEvent::ReasoningDelta(chunk) => json!({ "kind": "reasoning", "text": chunk }),

        SessionEvent::ToolInvocation(call) => json!({
            "kind": "tool-call",
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments_json,
        }),

        // `name` on both arms is what lets the transcript special-case a tool
        // whose call renders as a form rather than a row.
        SessionEvent::ToolResult(out) => json!({
            "kind": "tool-result",
            "id": out.call_id,
            "name": out.name,
            "ok": out.ok,
            "content": out.content,
        }),

        SessionEvent::Nudge(text) => json!({ "kind": "nudge", "text": text }),
        SessionEvent::SystemNote(text) => json!({ "kind": "note", "text": text }),
        SessionEvent::Incident(text) => json!({ "kind": "incident", "text": text }),

        SessionEvent::Modification(m) => json!({
            "kind": "modification",
            "aspect": m.aspect,
            "revision": m.revision,
            "ok": m.success,
            "detail": m.detail,
        }),

        SessionEvent::TurnStarted => json!({ "kind": "turn-started" }),

        SessionEvent::TurnFinished(stats) => json!({
            "kind": "turn-finished",
            "iterations": stats.iterations,
            "cost": stats.cost_usd,
            "prompt_tokens": stats.prompt_tokens,
            "completion_tokens": stats.completion_tokens,
            "tools": stats.tools_used,
            "stopped_by": stats.stopped_by,
        }),

        // Version-control activity on this conversation's sandbox branch:
        // trunk updates, resets, conflict handoffs. Inline in the transcript
        // so the code's history reads alongside the conversation's.
        SessionEvent::BranchOp(op) => json!({
            "kind": "branch-op",
            "op": op.op,
            "ok": op.ok,
            "from_rev": op.from_rev,
            "to_rev": op.to_rev,
            "conflicts": op.conflicts,
            "detail": op.detail,
        }),

        // Worth showing: the reader should be able to see that older messages
        // are now standing in summarized form, and roughly how much went.
        SessionEvent::ContextCompacted(c) => json!({
            "kind": "compacted",
            "replaced": c.messages_replaced,
            "tokens_before": c.tokens_before,
            "summary": c.summary,
        }),

        // Transient, like a token delta: the transcript draws one progress card
        // and updates it in place rather than appending a row per frame. The
        // `compacted` event above is what finally replaces it.
        SessionEvent::CompactionProgress(p) => json!({
            "kind": "compacting",
            "phase": p.phase,
            "span": p.span,
            "spans": p.spans,
            "messages": p.messages,
            "tokens_before": p.tokens_before,
            "tokens_target": p.tokens_target,
            "model": p.model,
            "detail": p.detail,
        }),
    };

    let obj = body.as_object_mut()?;
    obj.insert("type".into(), json!("event"));
    obj.insert("session".into(), json!(ev.session_id));
    obj.insert("seq".into(), json!(ev.seq));
    obj.insert("ts".into(), json!(ev.ts_ms));
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thetis::grip::types::{Author, UserMsg};

    fn outbound(event: SessionEvent) -> OutboundEvent {
        OutboundEvent {
            session_id: "s".into(),
            seq: Some(1),
            ts_ms: 0,
            event,
        }
    }

    #[test]
    fn a_user_frame_carries_its_author() {
        let frame = event(&outbound(SessionEvent::UserMessage(UserMsg {
            text: "hi".into(),
            attachments: vec![],
            author: Some(Author {
                id: "bob".into(),
                display: "Bob".into(),
                surface: "discord".into(),
            }),
        })))
        .expect("a user message renders");

        assert_eq!(frame["author"]["id"], json!("bob"));
        assert_eq!(frame["author"]["display"], json!("Bob"));
        // The surface reaches the view because a conversation someone can
        // reach two ways needs to say which way a message came in.
        assert_eq!(frame["author"]["surface"], json!("discord"));
    }

    #[test]
    fn an_unattributed_message_renders_a_null_author_not_a_missing_key() {
        // Every message from before authorship has none, as do the notes the
        // machine writes to a transcript itself. The view reads absence as "the
        // reader", which is what it assumed before the field existed — so this
        // must render rather than drop the frame.
        let frame = event(&outbound(SessionEvent::UserMessage(UserMsg {
            text: "from before".into(),
            attachments: vec![],
            author: None,
        })))
        .expect("an unattributed message still renders");

        assert_eq!(frame["text"], json!("from before"));
        assert_eq!(frame["author"], json!(null));
    }
}
