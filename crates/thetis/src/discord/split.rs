//! Laying a long reply out across several Discord messages.
//!
//! Discord rejects a body over 2000 characters. The connector used to answer
//! that by keeping the tail and stamping `… (truncated)` on the front, which
//! throws away the beginning of every long answer — and the beginning is where
//! the reasoning is.
//!
//! ## Why this is stateful and not just a chop every 2000 chars
//!
//! Markdown is not context-free across a cut. A page that ends inside a fenced
//! code block leaves the fence open, so Discord renders that message's tail as
//! code and the *next* message's prose as code too — everything after the cut is
//! wrong, not just the boundary. The same in reverse: a page that starts inside
//! a code block without reopening the fence shows the code as prose, complete
//! with the markdown inside it being interpreted.
//!
//! So the paginator carries the fence state across the break: it closes what is
//! open at the end of a page and reopens it, with the same info string, at the
//! top of the next one.
//!
//! ## Prefix stability, which is what makes it usable while streaming
//!
//! Pages are packed greedily from the start, so page N depends only on the text
//! before it. Appending to the text can therefore only change the *last* page —
//! every page before it is already final and never needs editing again. That is
//! what lets a streaming reply call this on every tick without rewriting
//! messages the reader has already scrolled past.

/// Discord's hard cap on a message body, in characters.
pub const LIMIT: usize = 2000;

/// Cost of the "\n```" a page must add when it ends inside a code block.
const CLOSE_COST: usize = 4;

/// The info string of a fence line, or `None` if the line is not a fence.
///
/// Only triple-backtick fences are recognised, because they are the only kind
/// Discord renders. A `~~~` line is ordinary text here and needs no carrying.
fn fence_of(line: &str) -> Option<&str> {
    Some(line.trim_start().strip_prefix("```")?.trim())
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Splits `text` into message bodies, each at most `limit` characters.
///
/// Returns an empty vector for empty text, and a single untouched page whenever
/// the whole thing fits — the overwhelming common case, which must not be
/// reshaped at all.
pub fn paginate(text: &str, limit: usize) -> Vec<String> {
    let text = text.trim_end();
    if text.is_empty() {
        return Vec::new();
    }
    if chars(text) <= limit {
        return vec![text.to_string()];
    }

    let mut pager = Pager {
        limit: limit.max(CLOSE_COST + 8),
        pages: Vec::new(),
        cur: String::new(),
        needs_sep: false,
        open: None,
        inline: false,
    };
    for line in text.split('\n') {
        pager.push_line(line);
    }
    pager.finish()
}

/// Greedy line-by-line packing that carries markup state across page breaks.
///
/// Two pieces of state survive a break, and both have to, in both directions —
/// closed at the end of the page and reopened at the top of the next.
struct Pager {
    limit: usize,
    pages: Vec<String>,
    /// The page being filled.
    cur: String,
    /// Whether a newline is owed before the next content. Tracked rather than
    /// derived from `cur.is_empty()`, because a page that has just reopened a
    /// fence is non-empty but its content must start on the line the fence
    /// already ended.
    needs_sep: bool,
    /// The info string of a fence left open at the end of `cur`, if any.
    open: Option<String>,
    /// Whether an inline `code` span is open at the end of `cur`. Only tracked
    /// outside a fenced block, where backticks are literal.
    inline: bool,
}

/// Whether a chunk of text leaves an inline span open, given it did not start
/// inside one. Fence lines are excluded by the caller.
fn flips_inline(text: &str) -> bool {
    text.matches('`').count() % 2 == 1
}

impl Pager {
    /// What a page must add to close the markup left open on it.
    fn reserve(open: bool, inline: bool) -> usize {
        (if open { CLOSE_COST } else { 0 }) + (if inline { 1 } else { 0 })
    }

    /// Seals the current page and opens the next one, carrying markup state.
    fn break_page(&mut self) {
        if !self.cur.trim().is_empty() {
            let mut page = std::mem::take(&mut self.cur);
            if self.inline {
                page.push('`');
            }
            if self.open.is_some() {
                page.push_str("\n```");
            }
            self.pages.push(page);
        }
        self.cur.clear();
        self.needs_sep = false;
        // Reopen what was open, or the next page renders in the wrong mode:
        // code as prose, or prose as code.
        if let Some(info) = self.open.clone() {
            self.cur = format!("```{info}\n");
        } else if self.inline {
            self.cur.push('`');
        }
    }

    fn push_line(&mut self, line: &str) {
        let fence = fence_of(line).map(str::to_string);
        // The fence state this line leaves behind: a fence line toggles,
        // anything else inherits.
        let after = match (&self.open, &fence) {
            (Some(_), Some(_)) => None,
            (None, Some(info)) => Some(info.clone()),
            (state, None) => state.clone(),
        };
        // Inline spans do not exist inside a fenced block, and a fence line's
        // own backticks are not an inline span.
        let inline_after = if after.is_some() || fence.is_some() {
            false
        } else {
            self.inline ^ flips_inline(line)
        };
        let reserve = Self::reserve(after.is_some(), inline_after);
        let sep = usize::from(self.needs_sep);

        if chars(&self.cur) + sep + chars(line) + reserve > self.limit {
            self.break_page();
        }
        // Still too long even on a fresh page: the line itself must be cut.
        let sep = usize::from(self.needs_sep);
        if chars(&self.cur) + sep + chars(line) + reserve > self.limit {
            self.push_wrapped(line);
            return;
        }
        if sep == 1 {
            self.cur.push('\n');
        }
        self.cur.push_str(line);
        self.needs_sep = true;
        self.open = after;
        self.inline = inline_after;
    }

    /// Cuts a line too long to fit any page and pushes the pieces.
    ///
    /// Breaks at whitespace where there is any, so a wall of prose is not
    /// severed mid-word. Each piece updates the inline state, so `break_page`
    /// closes and reopens exactly what is actually open.
    fn push_wrapped(&mut self, line: &str) {
        let mut rest: Vec<char> = line.chars().collect();
        let in_code = self.open.is_some();
        while !rest.is_empty() {
            let sep = usize::from(self.needs_sep);
            // Reserve for closing a fence, plus one for an inline span that
            // this piece might yet open.
            let reserve = Self::reserve(in_code, !in_code);
            let room = self
                .limit
                .saturating_sub(chars(&self.cur) + sep + reserve);
            if room < 16 {
                self.break_page();
                continue;
            }
            let take = room.min(rest.len());
            let mut cut = take;
            if take < rest.len() {
                if let Some(space) = rest[..take].iter().rposition(|c| c.is_whitespace()) {
                    if space > take / 2 {
                        cut = space;
                    }
                }
            }
            let piece: String = rest[..cut].iter().collect();
            rest.drain(..cut);
            while rest.first().is_some_and(|c| c.is_whitespace()) {
                rest.remove(0);
            }
            if self.needs_sep {
                self.cur.push('\n');
            }
            self.cur.push_str(&piece);
            self.needs_sep = true;
            if !in_code {
                self.inline ^= flips_inline(&piece);
            }
            if !rest.is_empty() {
                self.break_page();
            }
        }
    }

    fn finish(mut self) -> Vec<String> {
        if !self.cur.trim().is_empty() {
            let mut page = std::mem::take(&mut self.cur);
            // The last page closes what is open too: the source text may itself
            // have ended inside an unterminated block, and leaving it open
            // would swallow later messages in the channel.
            if self.inline {
                page.push('`');
            }
            if self.open.is_some() {
                page.push_str("\n```");
            }
            self.pages.push(page);
        }
        self.pages
    }
}

/// One change to make to the channel to bring it in line with the new text.
#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    /// Edit the message already at this page position to the new body.
    Edit { page: usize },
    /// Post a new message for this page position.
    Send { page: usize },
    /// Remove the message at this position: the reply now needs fewer pages.
    Delete { page: usize },
}

/// Works out the smallest set of Discord calls that turns `sent` into `want`.
///
/// The point is to send as little as possible. A streaming reply calls this on
/// every tick, and because pagination is prefix-stable only the tail actually
/// differs, so an unchanged page produces no call at all. Editing every page
/// each tick would multiply the request count by the page count and hit the
/// per-channel rate limit on any long answer.
///
/// Deletes come back in descending order so applying them in sequence cannot
/// invalidate the positions of the ones still to do.
pub fn plan(sent: &[String], want: &[String]) -> Vec<Op> {
    let mut ops = Vec::new();
    for (page, body) in want.iter().enumerate() {
        match sent.get(page) {
            Some(old) if old == body => {}
            Some(_) => ops.push(Op::Edit { page }),
            None => ops.push(Op::Send { page }),
        }
    }
    for page in (want.len()..sent.len()).rev() {
        ops.push(Op::Delete { page });
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common case. A reply that fits is one message, byte for byte.
    #[test]
    fn a_short_reply_is_one_untouched_page() {
        let pages = paginate("hello **there**", LIMIT);
        assert_eq!(pages, vec!["hello **there**".to_string()]);
    }

    #[test]
    fn nothing_at_all_is_no_pages() {
        assert!(paginate("", LIMIT).is_empty());
        assert!(paginate("   \n\n ", LIMIT).is_empty());
    }

    /// The regression this replaces: the old path kept the tail and stamped
    /// "… (truncated)" on the front, so the opening of every long answer was
    /// simply lost. Every character must now survive somewhere.
    #[test]
    fn nothing_is_dropped_from_a_long_reply() {
        let body: String = (1..=400).map(|i| format!("line {i} of the answer\n")).collect();
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1, "should have split: {}", pages.len());
        let rejoined = pages.join("\n");
        for i in [1, 137, 400] {
            assert!(
                rejoined.contains(&format!("line {i} of the answer")),
                "line {i} went missing"
            );
        }
        assert!(
            rejoined.starts_with("line 1 of the answer"),
            "the beginning must survive, not just the tail"
        );
    }

    #[test]
    fn every_page_is_within_discords_limit() {
        let body: String = (1..=600).map(|i| format!("some prose on line {i}\n")).collect();
        for page in paginate(&body, LIMIT) {
            assert!(chars(&page) <= LIMIT, "page of {} chars", chars(&page));
        }
    }

    /// The heart of it: a break inside a fenced block must close and reopen the
    /// fence, or Discord renders the following message as code — and every
    /// message after that one too.
    #[test]
    fn a_code_block_is_closed_and_reopened_across_a_break() {
        let mut body = String::from("Here is the code:\n\n```rust\n");
        for i in 0..200 {
            body.push_str(&format!("    let x{i} = compute(x{i});\n"));
        }
        body.push_str("```\n");
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        for (i, page) in pages.iter().enumerate() {
            let fences = page.matches("```").count();
            assert_eq!(
                fences % 2,
                0,
                "page {i} leaves a fence open:\n{}",
                &page[page.len().saturating_sub(120)..]
            );
        }
        for page in &pages[1..] {
            assert!(
                page.starts_with("```rust"),
                "a continued block must reopen with its language: {:?}",
                &page[..20.min(page.len())]
            );
        }
    }

    /// The info string is what makes the highlighting continue. Losing it turns
    /// the second half of a listing into an unhighlighted block.
    #[test]
    fn the_language_is_carried_not_guessed() {
        let mut body = String::from("```python\n");
        for i in 0..300 {
            body.push_str(&format!("value_{i} = fetch({i})\n"));
        }
        body.push_str("```");
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        assert!(pages[1].starts_with("```python"));
    }

    /// Text outside a fence must not gain one.
    #[test]
    fn prose_pages_are_not_wrapped_in_code() {
        let body: String = (1..=300).map(|i| format!("prose paragraph {i}\n\n")).collect();
        for page in paginate(&body, LIMIT) {
            assert!(!page.starts_with("```"), "prose page opened a code block");
        }
    }

    /// A block that ends before the break is not reopened afterwards.
    #[test]
    fn a_closed_block_does_not_leak_into_later_pages() {
        let mut body = String::from("```\nshort block\n```\n\n");
        for i in 0..300 {
            body.push_str(&format!("then a lot of prose, line {i}\n"));
        }
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        assert!(
            !pages[1].starts_with("```"),
            "the block was already closed: {:?}",
            &pages[1][..20.min(pages[1].len())]
        );
    }

    /// Breaks land on line boundaries, so a sentence is not severed when there
    /// is anywhere better to cut.
    #[test]
    fn pages_break_between_lines() {
        let body: String = (1..=300).map(|i| format!("line {i}\n")).collect();
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        for page in &pages {
            assert!(
                page.trim_end().ends_with(|c: char| c.is_ascii_digit()),
                "a page ended mid-line: {:?}",
                &page[page.len().saturating_sub(30)..]
            );
        }
    }

    /// One enormous line — a pasted log, a minified blob — still has to be
    /// delivered, and cutting it mid-word is the last resort, not the first.
    #[test]
    fn a_single_overlong_line_is_wrapped_at_whitespace() {
        let body = "word ".repeat(1200);
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        for page in &pages {
            assert!(chars(page) <= LIMIT);
            assert!(
                !page.contains("wor\n") && !page.trim_end().ends_with("wor"),
                "cut mid-word with spaces available"
            );
        }
    }

    /// No whitespace anywhere: it must still terminate and stay in bounds
    /// rather than loop or overflow.
    #[test]
    fn an_unbreakable_line_is_still_delivered() {
        let body = "x".repeat(7000);
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() >= 4);
        let total: usize = pages.iter().map(|p| p.matches('x').count()).sum();
        assert_eq!(total, 7000, "characters were lost");
    }

    /// Multi-byte text must not be cut through a character.
    #[test]
    fn a_multibyte_reply_is_split_on_character_boundaries() {
        let body = "日本語のテキスト ".repeat(500);
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        for page in &pages {
            assert!(chars(page) <= LIMIT);
            assert!(page.is_char_boundary(page.len()));
        }
    }

    /// Appending — which is what streaming does — must not disturb pages the
    /// reader has already been shown. Without this the connector would rewrite
    /// every message on every tick.
    #[test]
    fn earlier_pages_do_not_move_as_text_is_appended() {
        let mut body: String = (1..=300).map(|i| format!("line {i}\n")).collect();
        let first = paginate(&body, LIMIT);
        assert!(first.len() >= 2);
        for i in 301..=360 {
            body.push_str(&format!("line {i}\n"));
        }
        let second = paginate(&body, LIMIT);
        assert!(second.len() >= first.len());
        for (n, page) in first.iter().enumerate().take(first.len() - 1) {
            assert_eq!(
                page, &second[n],
                "page {n} changed when text was appended after it"
            );
        }
    }

    /// A reply that fits is passed through untouched even when its own markup
    /// is unbalanced — a model still streaming mid-listing, say. Discord
    /// renders each message on its own, so an unclosed fence affects only that
    /// message and resolves itself as the rest of the text arrives; "repairing"
    /// it would mean every streaming tick rewrote the body and the reader
    /// watched fences flicker in and out.
    #[test]
    fn a_reply_that_fits_is_never_reshaped() {
        let mid_stream = "here:\n```rust\nfn main() {}";
        assert_eq!(paginate(mid_stream, LIMIT), vec![mid_stream.to_string()]);
    }

    /// Balance is a property of the *pages this splitter creates*, because a
    /// fence it opened to continue a block would otherwise be left hanging.
    #[test]
    fn a_split_block_leaves_no_page_unbalanced() {
        let mut body = String::from("```rust\n");
        for i in 0..200 {
            body.push_str(&format!("    let x{i} = compute(x{i});\n"));
        }
        // Deliberately unterminated in the source.
        for page in paginate(&body, LIMIT) {
            assert_eq!(
                page.matches("```").count() % 2,
                0,
                "a page this splitter produced leaves a fence open"
            );
        }
    }

    /// The streaming case: an unchanged page must produce no request. Editing
    /// every page on every tick would multiply requests by page count.
    #[test]
    fn an_unchanged_page_costs_no_request() {
        let sent = vec!["one".to_string(), "two".to_string()];
        assert!(plan(&sent, &sent).is_empty());
    }

    /// Only the last page moves while streaming, so only it is edited, and a
    /// newly needed page is sent rather than replacing anything.
    #[test]
    fn only_the_growing_tail_is_touched() {
        let sent = vec!["one".to_string(), "two".to_string()];
        let want = vec!["one".to_string(), "two and more".to_string(), "three".to_string()];
        assert_eq!(
            plan(&sent, &want),
            vec![Op::Edit { page: 1 }, Op::Send { page: 2 }]
        );
    }

    /// Nothing sent yet: every page is a send, in reading order, so the channel
    /// shows them the right way round.
    #[test]
    fn a_first_flush_sends_every_page_in_order() {
        let want = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            plan(&[], &want),
            vec![Op::Send { page: 0 }, Op::Send { page: 1 }, Op::Send { page: 2 }]
        );
    }

    /// A reply can shrink — `AssistantMessage` replaces a step's streamed text,
    /// and the retractable tool note comes off at the end. Leftover messages
    /// have to be deleted, back to front so earlier indices stay valid.
    #[test]
    fn a_shrunken_reply_deletes_its_leftovers_back_to_front() {
        let sent = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let want = vec!["a".to_string()];
        assert_eq!(
            plan(&sent, &want),
            vec![Op::Delete { page: 2 }, Op::Delete { page: 1 }]
        );
    }

    /// Dumps the real paginator's output for a live check against Discord, so
    /// what gets posted to a channel is what this code produces rather than a
    /// hand-made facsimile. Ignored by default; see
    /// `discord-connector/live-probing`.
    ///
    /// `cargo test -p thetis --lib -- --ignored dump_pages_for_a_live_probe`
    #[test]
    #[ignore]
    fn dump_pages_for_a_live_probe() {
        let mut body = String::from(
            "Here is a long answer with markup that spans messages.\n\n\
             **1. A heading with `inline code` in it.**\n\n\
             ```rust\n",
        );
        for i in 0..160 {
            body.push_str(&format!(
                "    let value_{i} = compute(&input_{i}, /* index */ {i});\n"
            ));
        }
        body.push_str("```\n\nAnd prose after the block, to check it closed.\n");
        for i in 0..40 {
            body.push_str(&format!("- bullet {i} with `code` and **bold**\n"));
        }
        let pages = paginate(&body, LIMIT);
        let json = serde_json::to_string(&pages).unwrap();
        std::fs::write("/tmp/pages.json", json).unwrap();
        eprintln!("wrote {} pages to /tmp/pages.json", pages.len());
    }

    /// An inline span cut in half would apply its formatting to everything
    /// after the cut.
    #[test]
    fn a_dangling_inline_span_is_closed_and_reopened() {
        let body = format!("`{}`", "a".repeat(4000));
        let pages = paginate(&body, LIMIT);
        assert!(pages.len() > 1);
        for (i, page) in pages.iter().enumerate() {
            assert_eq!(
                page.matches('`').count() % 2,
                0,
                "page {i} leaves an inline span open"
            );
        }
    }
}
