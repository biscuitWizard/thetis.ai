---
name = "File mentions and attachments"
brief = "How @-mentions carry workspace file contents from the composer to the model, and the traps in that path."
when_to_use = "Use when changing the composer's @-mention menu, the attachment tray, the highlight mirror, or anything about how a file's contents reach the model — including the workspace-find frame, user_content inlining, and the attachment limits. Also use when a mention does not highlight, attaches nothing, attaches the wrong bytes, or when a pasted path behaves differently from one picked from the menu. Not for the Files rail tab's own editor, and not for image attachments alone."
tags = ["composer", "attachments", "mentions", "workspace", "context"]
version = 1
---

# File mentions and attachments

Typing `@` in the composer opens a menu of real shared-workspace paths; choosing
one attaches that file so its **contents** reach the model. This is the whole
path, and the places it goes wrong.

## The pipeline, end to end

| Stage | Where | What it does |
|---|---|---|
| Search | `crates/thetis/src/workspace_api.rs` — `workspace-find` | Fuzzy-matches a cached path index, host-side |
| Menu, highlight, reading | `ui/views/mentions.js` | Owns resolution and turns a path into an attachment |
| Composer | `ui/views/composer.js` | Awaits the reads, appends to `store.attachments`, sends |
| Wire | `handlers.rs::send` → `grip.rs::submit` | `{name, mime, data}` → `Attachment` |
| Transcript | `ui/views/transcript.js` | Names files under `files attached (n):` |
| The model | `agents/agent-core/src/lib.rs` — `user_content` | Inlines text as `<attached-file path="…">` |

**Cursor's `@file` inlines contents, not a path**, and that is what this copies.
A path alone tells the model nothing it can read without a tool call.

## Load-bearing decisions

- **Search is host-side, not a browser crawl.** The real workspace here is ~37k
  paths across several checkouts. An earlier version walked it over
  `workspace-list` from the browser; it was hopeless. The host keeps an index
  with a 45s TTL, invalidated on every mutation and upload.
- **The message text is the single source of truth.** There is no separate
  chip-list model to drift out of sync. `found(text)` scans the tokens and
  drives *both* the highlight and the attachments, so the two cannot disagree.
- **Attachment `name` is `workspace/<rel>`** — exactly the spelling the agent's
  own file tools take, so a truncated attachment can be read the rest of the way
  with `read_path`.
- **A folder attaches a listing, never its contents.** Recursing would spend the
  whole context on one mention.
- **Inlining is budgeted** in `user_content`: `MAX_INLINE_FILE_CHARS` per file
  and `MAX_INLINE_TOTAL_CHARS` shared across the message. Over budget, or not
  UTF-8, and the file is *named* with a hint to `read_path` rather than inlined
  as mojibake. Truncation says so in the text.
- **The mention text stays visible.** The user sees `@moor/README.md` in the sent
  message, highlighted; the contents ride along invisibly, like an image.

## Traps

1. **A pasted path is not a picked one.** Paths become known only by appearing
   in a search answer or a listing. A mention typed or pasted in full never
   opened the menu, so it resolved to nothing and **attached nothing** — while
   still looking like ordinary text, so nothing announced the failure. This is
   the likeliest way to write a mention once you know the path. `learn(text)`
   fixes it by listing the token's parent directory; it runs debounced on input
   and again, decisively, inside `attachmentsFor` before the message is sent.
   Any new way for text to arrive in the composer must reach `learn`.
2. **A listing does not prove the listed directory exists** — its entries name
   only children. `onList` must also remember `frame.path` itself, or
   `@moor/crates/` can never resolve from a listing of itself.
3. **Enter is overloaded.** With the menu open it completes; otherwise it sends.
   `mentions.handleKey` gets first refusal in the composer's `keydown` and
   returns true when it consumed the key.
4. **Choosing a directory keeps the menu open**, descending into it; choosing a
   file closes it. A test asserting "the menu closes after choosing" fails
   correctly on a directory.
5. **Submit has to wait on the network.** Reads start at pick time, but a send
   can outrun them, so `submit` is async with a `reading` guard that disables the
   send button. Declare the guard beside `locked()` or you get a TDZ error.
6. **Forgetting `missing`.** Tokens the host denied are cached so a typo is asked
   about once; that set must be cleared on mutation and invalidation, or a file
   created a moment ago can never resolve.
7. **The highlight is a mirror div, not styled text.** A textarea cannot carry
   spans. The mirror sits behind with transparent glyphs and identical font,
   padding and width; the visible text is still the textarea's, which keeps the
   caret and spellcheck native. Any font or padding change must apply to both —
   assert `dx`/`dy` ≤ 2 and matching computed font.
8. **`limits.max_attachments` is a real ceiling** and needs a restart to change.
   A dozen mentions plus images will hit it.

## Verifying it

`/preview/` is not enough: `workspace-find` is dispatched in the **gateway**
process, which runs trunk's binary, so on the live port the frame is silently
dropped and the menu just never opens. Use a second gateway
(`web-ui-design/verifying-on-a-branch`) with `THETIS_WORKSPACE_DIR` pointed at
the real workspace so search has something to find.

The check that actually proves it: hook `WebSocket.prototype.send` via
`Page.addScriptToEvaluateOnNewDocument`, capture the outgoing `send` frame, and
compare `base64.b64decode(attachment.data)` against
`GET /workspace/file/<path>` byte for byte. A SHA match is the claim; a chip
appearing in the tray is not. This is what caught trap 1 — the UI looked
perfect and the wire carried `attachments: []`.

Transcript rows are `.row.user`, not `.msg.user`.
