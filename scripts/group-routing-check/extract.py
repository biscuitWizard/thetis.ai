#!/usr/bin/env python3
"""Lift the pure parts of groups.rs into a standalone crate so the group table
and the router can be tested without a wasm host.

Copies the real source rather than restating it: a test that retyped the table
would only prove the copy self-consistent."""
import re, sys

src = open(sys.argv[1]).read()

# Top-level items, by the name they are declared with.
WANTED = [
    "const UNGROUPED",
    "pub struct ToolGroup",
    "pub fn all()",
    "fn find(",
    "pub fn all_ids()",
    "const PREFIX_RULES",
    "const COMPONENT_CAP_PREFIX",
    "pub fn component_group(",
    "pub fn builtin_active(",
    "fn tokens(",
    "pub fn score(",
    "pub fn coverage_gaps(",
    "fn in_table_order(",
]

lines = src.split("\n")
out = []
for want in WANTED:
    start = None
    for i, line in enumerate(lines):
        if line.startswith(want):
            start = i
            break
    if start is None:
        sys.exit(f"not found: {want}")
    # Walk back over the doc comment so the extracted item keeps its rationale.
    head = start
    while head > 0 and (lines[head - 1].startswith("///") or lines[head - 1].startswith("//")):
        head -= 1
    # A const ends at its first line ending in ';', or at a closing bracket.
    end = start
    if want.startswith("const") and lines[start].rstrip().endswith(";"):
        end = start
    else:
        depth = 0
        seen = False
        for i in range(start, len(lines)):
            depth += lines[i].count("{") + lines[i].count("[") - lines[i].count("}") - lines[i].count("]")
            if "{" in lines[i] or "[" in lines[i]:
                seen = True
            if seen and depth <= 0:
                end = i
                break
    out.append("\n".join(lines[head:end + 1]))

body = "\n\n".join(out)
# Private in groups.rs, where the module boundary is the whole crate; the
# harness imports them across a module, so promote rather than restate.
body = re.sub(r"^(const|fn) ", r"pub \1 ", body, flags=re.M)
# sys::log is a host import; in the harness a gap should be loud, not absent.
body = re.sub(r"sys::log\(\s*LogLevel::\w+,", "log_warn(", body)
print("fn log_warn(msg: &str) { println!(\"[log] {}\", msg.trim()); }\n")
print(body)
