#!/usr/bin/env python3
"""Freeze a skills corpus into a fixture the benchmark can replay forever.

Why this exists
---------------
The benchmark grades a ranker against a set of skill cards. If those cards are
whatever happens to be in `skills/` at the moment of the run, the score moves
for two unrelated reasons: the ranking code changed, or somebody added a skill.
Thetis pushes selectively to the remote, so different checkouts legitimately
carry different corpora -- the same commit measured here and in CI produced
corpus=127 and corpus=61, hit@1 0.750 and 0.583. That is not a bug in the
ranker, and a chart that mixes the two teaches nothing.

So the corpus becomes a committed fixture. Every revision is then graded on
identical cards with identical queries, and a movement in the number has
exactly one possible cause: the code.

What gets frozen
----------------
Only the fields the ranker actually reads. `Skill::index_text()` is

    name \n brief \n when_to_use [\n tags joined by spaces]

The body is excluded from ranking, so the body is excluded from the fixture.
That keeps it small, keeps private prose out of a committed file, and makes it
obvious to a reader that editing a body cannot change a score.

Structure is preserved, because ranking depends on it: parent absorption and
child promotion need the real parent/child edges, so the fixture is emitted as
a directory tree of frontmatter-only SKILL.md files that `skills::discover`
reads exactly as it reads the live tree.

Excluding private skills
------------------------
`--exclude` drops a subtree. Anything unpushed (torchship) must be excluded, or
the fixture cannot be committed and the gold set that references it cannot be
scored anywhere but one laptop.

Usage
-----
    ./pin-corpus.py --from ../../skills --out corpus/v1 --exclude torchship
    ./pin-corpus.py --from ../../skills --out corpus/v1 --exclude torchship --check
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys

RESERVED = {"references", "scripts", "assets"}
# Mirrors MAX_DEPTH in crates/thetis/src/skills.rs. A skill deeper than this is
# skipped by discover(), so freezing it would put a card in the fixture that the
# real loader would never surface.
MAX_DEPTH = 3


def split_frontmatter(text: str) -> tuple[str, bool]:
    """Return the raw TOML frontmatter, and whether a fence was present.

    Deliberately mirrors parse_frontmatter in skills.rs: strip a BOM, normalise
    CRLF, require the opening `---\\n`, close on the first `\\n---`.
    """
    text = text.replace("\r\n", "\n")
    if text.startswith("\ufeff"):
        text = text[1:]
    if not text.startswith("---\n"):
        return "", False
    rest = text[4:]
    idx = rest.find("\n---")
    if idx < 0:
        # An unclosed fence is a truncated file. skills.rs treats it as an
        # error; we skip it rather than emit a card the loader would reject.
        return "", False
    return rest[:idx], True


def iter_skills(root: str, excludes: list[str]):
    """Walk a skills tree the way discover() does, yielding (relpath, text).

    Two shapes are skills: a bare `*.md` that is not SKILL.md, and a directory
    containing SKILL.md. Reserved directory names are not skills.
    """
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in RESERVED)
        rel_dir = os.path.relpath(dirpath, root)
        rel_dir = "" if rel_dir == "." else rel_dir

        depth = 0 if not rel_dir else rel_dir.count(os.sep) + 1
        if depth >= MAX_DEPTH:
            dirnames[:] = []

        for name in sorted(filenames):
            if not name.endswith(".md"):
                continue
            rel = os.path.join(rel_dir, name) if rel_dir else name
            skill_id = os.path.dirname(rel) if name == "SKILL.md" else rel[:-3]
            if any(
                skill_id == e or skill_id.startswith(e + "/") for e in excludes
            ):
                continue
            if name != "SKILL.md" and depth >= MAX_DEPTH:
                continue
            with open(os.path.join(dirpath, name), encoding="utf-8") as fh:
                yield rel, fh.read()


def build(src: str, excludes: list[str]) -> dict[str, str]:
    """Map relpath -> frontmatter-only file contents."""
    out: dict[str, str] = {}
    skipped: list[str] = []
    for rel, text in iter_skills(src, excludes):
        fm, fenced = split_frontmatter(text)
        if not fenced or not fm.strip():
            # No frontmatter means no card text: the ranker sees an empty
            # name/brief/when_to_use. Such a skill is unrankable, and lint
            # flags it separately. Recording it would pad the corpus with a
            # card that cannot be retrieved.
            skipped.append(rel)
            continue
        out[rel] = f"---\n{fm}\n---\n"
    if skipped:
        print(
            f"  note: {len(skipped)} file(s) had no frontmatter and were left out",
            file=sys.stderr,
        )
        for s in skipped[:5]:
            print(f"    {s}", file=sys.stderr)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--from", dest="src", required=True, help="a skills/ directory")
    ap.add_argument("--out", required=True, help="fixture directory to write")
    ap.add_argument(
        "--exclude",
        action="append",
        default=[],
        help="skill id prefix to drop, e.g. torchship. Repeatable.",
    )
    ap.add_argument(
        "--check",
        action="store_true",
        help="verify the fixture matches --from instead of writing; exit 1 on drift",
    )
    args = ap.parse_args()

    if not os.path.isdir(args.src):
        print(f"no such directory: {args.src}", file=sys.stderr)
        return 1

    built = build(args.src, args.exclude)
    if not built:
        print("refusing to write an empty fixture", file=sys.stderr)
        return 1

    if args.check:
        existing: dict[str, str] = {}
        for dirpath, _dirs, files in os.walk(args.out):
            for name in files:
                if name.endswith(".md"):
                    p = os.path.join(dirpath, name)
                    with open(p, encoding="utf-8") as fh:
                        existing[os.path.relpath(p, args.out)] = fh.read()
        added = sorted(set(built) - set(existing))
        removed = sorted(set(existing) - set(built))
        changed = sorted(
            k for k in set(built) & set(existing) if built[k] != existing[k]
        )
        if not (added or removed or changed):
            print(f"fixture is current: {len(built)} skills")
            return 0
        print(f"fixture has drifted from {args.src}:", file=sys.stderr)
        for label, items in (
            ("only in skills/", added),
            ("only in fixture", removed),
            ("frontmatter differs", changed),
        ):
            for i in items:
                print(f"  {label}: {i}", file=sys.stderr)
        print(
            "\nThis is expected after editing a brief. Re-pin deliberately and in\n"
            "its own commit, so the chart shows where the corpus changed:\n"
            f"  ./pin-corpus.py --from {args.src} --out {args.out}"
            + "".join(f" --exclude {e}" for e in args.exclude),
            file=sys.stderr,
        )
        return 1

    if os.path.isdir(args.out):
        shutil.rmtree(args.out)
    for rel, text in sorted(built.items()):
        dest = os.path.join(args.out, rel)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, "w", encoding="utf-8") as fh:
            fh.write(text)

    total = sum(len(t.encode()) for t in built.values())
    print(f"pinned {len(built)} skills into {args.out} ({total / 1024:.0f} KB)")
    if args.exclude:
        print(f"  excluded: {', '.join(args.exclude)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
