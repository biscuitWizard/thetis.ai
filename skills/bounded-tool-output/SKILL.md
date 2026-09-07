---
name = "Bounding what a tool returns"
brief = "Cap a tool's output so it always says how to get the rest, and never cut off the line that says how."
when_to_use = "Use when a tool result comes back truncated, huge, or ending in a dead-end note; when adding or reviewing a tool that reads a file, lists rows, dumps JSON or scrapes a page; and when deciding between paginating, searching and summarising. Use it too when a result is bloated by nested JSON wrappers or an envelope, or when a tool's limit argument seems to be ignored. Not for prose summarisation of web pages, which is web-summarize's job."
universal = false
tags = ["tools", "truncation", "pagination", "output", "context", "limits", "json", "verbosity"]
version = 2
---

# Bounding what a tool returns

A tool that can return a lot must return a little and say how to get the rest.
Getting this wrong wastes thousands of tokens on output nobody can use, and the
failure is silent: the result looks like an answer.

## The one rule

**The recovery instruction goes last, and something must guarantee it survives.**

Every layer cuts from somewhere. The kernel's cap keeps the head and the tail
(`crates/thetis/src/spill.rs`). A tool's own cap usually keeps only the head. So a
footer written at the end of the text is destroyed by any head-keeping cut above
it — which is exactly the cut that made the footer necessary.

## The unit-mismatch bug

This is the bug, and it recurs. A tool bounds its output in one unit while the
layer above bounds in another:

| Tool bounds by | Host bounds by | What happens |
|---|---|---|
| lines (2000) | bytes (32768) | Wide lines: tool thinks it returned everything, writes no footer, host cuts blind |
| chars per page × 10 pages | bytes, total | Each page fits, the sum does not; last pages vanish |
| nothing | bytes | Guaranteed dead end |

The tool believes it succeeded, so it emits no continuation hint, and the cut
lands where a hint would have been. **Bound by the same unit the layer above
uses, or by both.**

```rust
// Wrong: a line limit alone.
let body = lines[start..start + limit].join("\n");

// Right: whichever comes first, and the footer states which.
const BYTE_BUDGET: usize = 28_000;
let mut end = start;
for (i, l) in lines[start..want_end].iter().enumerate() {
    let row = format!("{:>6}\t{}\n", start + i + 1, l);
    if !body.is_empty() && body.len() + row.len() > BYTE_BUDGET {
        stopped_early = true;
        break;
    }
    body.push_str(&row);
    end = start + i + 1;
}
```

## First, stop paying for noise

Before choosing how to cut, check whether the output is *dense*. A cut is a
loss; deleting something that carries no information is free. Machine-generated
JSON is the usual offender — envelopes, union tags and type wrappers around
every scalar:

```json
{"names":[{"value":"aliases"}],"location":{"obj":{"ObjId":{"id":1}}}}
{"names":["aliases"],"location":"#1"}
```

Those two lines say the same thing. The moo tools pretty-printed the first form
and a 45-verb list came to 40,968 bytes, overran the cap, and arrived cut; the
second form is 11,257 bytes and complete. **No pagination was needed at all** —
the tool had simply been quoting the transport instead of answering.

Two rules make such a transform safe:

- **Find a structural discriminator, do not hardcode a key list.** In that
  protocol the union tags are CamelCase and the real field names are snake_case,
  so "unwrap a single-key object whose key is CamelCase" is a rule that keeps
  working as the protocol grows.
- **Keep what you do not recognise.** An unknown tag must survive rather than be
  dropped silently, so a protocol addition shows up as odd output instead of a
  field that quietly went missing.

Then prove it is lossless rather than assuming: enumerate every scalar and every
informative field name on both sides and compare. Against real captured
responses, not invented fixtures — the point of the transform is knowing which
keys are plumbing, and only live data settles that.

Also worth checking while you are there: a renderer that drops information often
has a matching parser that will not take it back. If a listing prints an id,
make sure some tool accepts that spelling as input.

## Pick a strategy

Once the output is dense, in order of preference:

1. **A window with an offset.** For anything with a natural sequence — file
   lines, rows, characters of a document. Cheapest, exact, and the caller
   controls the cost. `read_path`'s `offset`/`limit` is the model.
2. **A search.** Usually the question the caller actually has: "where is the verb
   `look`", not "the first 400 lines". Add it wherever a caller would otherwise
   page through looking for something. Returns tens of lines instead of
   thousands.
3. **A handle to fetch later.** When the work is already done and re-running costs
   money: `bq-query` returns 200 rows and a job id for `bq-results`.
4. **Spill to a file.** When output cannot be bounded at the source. Write it
   under `/workspace/` and return head + tail + path; `read_path` and
   `search_files` already window and grep. This is the kernel's fallback for any
   tool that overruns.
5. **Summarise with an LLM.** Last resort. Costs a call, is lossy, and is wrong
   for code, JSON and logs where the caller needs exact text. Fits prose only.

## Writing the footer

State what you returned, that there is more, and the exact argument to continue:

```
[lines 104-206 of 400; read on with offset 207]
[lines 1-103 of 400 (stopped at the output size limit); read on with offset 104]
[characters 0-18000 of 96000. Read on with offset 18000, or pass find to jump to the part you want.]
```

- Name the **next** offset, not the last one shown. Off-by-one here makes the
  caller re-read a line or skip one.
- Say **why** it stopped when it was not the caller's limit — "stopped at the
  output size limit" tells them raising `limit` will not help.
- Never end at a bare `[truncated]`. A caller told only that the answer is
  incomplete will either guess or re-run the same call.

## A confirmation is not a read

A write tool that echoes the whole object back is spending a read's budget to
answer yes/no. Cap the echo hard and name the tool that reads properly:

```
[4000 of 96000 characters shown — this is a confirmation of the write, not the
whole page. Read the rest with notion-page-get, which takes
content_offset/content_limit and find.]
```

## Summarise a list, fetch one item

When listing things whose values are large — cookies, localStorage, env vars,
blobs — return the keys with sizes and let the caller ask for one:

```js
o[k] = v.length > 300
  ? `${v.slice(0, 300)}… [${v.length} chars; use action 'get' with this key]`
  : v;
```

## A silent limit is a dead end too

A tool that accepts `limit` and applies it without saying so is the same bug
wearing a different hat: the caller cannot tell a complete answer from a trimmed
one. Say what was dropped and how to see it.

Test the limit against a *real* response, because a recursive walker is easy to
get subtly wrong. `moo-list-objects` truncated the first array it found, but the
list it was meant to trim sat two levels down under `results[].result.objects`,
so `limit=3` returned 33 objects for months. A later attempt recursed only into
arrays holding containers, which broke on the same endpoint the moment the list
mixed plain ids with wrapped ones. The rule that worked was the boring one:
apply the limit to every list, and report the longest.

## Checking a tool you did not write

1. Find the output path. What is the largest thing it can return? A whole file, a
   page's DOM, an unbounded `JSON.stringify`?
   Ask also whether what it returns is *dense* — see above. A wrapper-heavy
   payload may need no cap once it stops quoting its transport.
2. Is there a cap? If not, the host's cap applies and the result is a dead end.
3. If there is a cap, **in what unit**, and does it match the host's bytes?
4. Does the text past the cap name a way to continue? Is that text at the end,
   where a head-keeping cut removes it?
5. Force it over the limit and read what comes back. Do not reason about it —
   the whole class of bug is that the code looks correct.

```bash
# Make something too big, then call the tool on it and read the tail.
python3 -c "open('/tmp/wide.txt','w').write(''.join('line%03d '%i+'a'*282+'\n' for i in range(1,401)))"
```

## Known gaps

- `bounded()` in the moo tools keeps only the head; its note names a narrower
  request and the spill path rather than offering an offset, because most of
  those tools have no natural sequence to offset into. Since `compact_wire`
  landed, the responses that used to overrun it mostly fit, so the cap is now a
  backstop rather than the normal path.
- The playwright sidecar runs from the deployed tree, not a worktree, so its
  bounds cannot be verified from a branch — only after merge.
- Terminal output is capped twice, with opposite conventions: `terminal.rs::clip`
  keeps the **tail** at 64 KiB, then the tool-output cap keeps head and tail at
  32 KiB.
