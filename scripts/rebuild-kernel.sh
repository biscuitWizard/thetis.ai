#!/usr/bin/env bash
#
# Brings the orchestrator binary up to date with the checkout, then gets out of
# the way. Wired in as the unit's ExecStartPre, so `systemctl restart thetis`
# picks up kernel edits the agent has committed without anyone remembering a
# separate build step.
#
# It never fails the start. A broken build, a full disk, or a fresh binary that
# will not answer the probe all leave the previous binary in place and exit 0:
# the last good kernel serving is better than the unit refusing to come up.
set -uo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd) || exit 0
binary=$root/target/release/thetis
pinned=$binary.prestart

say() { echo "rebuild-kernel: $*"; }  # ExecStartPre output lands in the journal

cd -- "$root" || { say "no $root; nothing to build"; exit 0; }

# What the binary is right now. cargo replaces it by unlinking and writing a new
# file, so the inode moving is what says "this was rebuilt".
ident() { stat -c '%i:%Y' -- "$1" 2>/dev/null; }
before=$(ident "$binary")

# A hard link costs no space and pins the *inode*, the same trick the gateway
# uses to keep its own build reachable: after cargo replaces the file, this
# entry still names the bytes that were serving, so a build that compiles but
# will not start can be undone.
rm -f -- "$pinned"
if [[ -e $binary ]] && ! ln -- "$binary" "$pinned" 2>/dev/null; then
    cp -p -- "$binary" "$pinned" || say "could not pin the current binary"
fi

# cargo is the authority on whether anything actually needs rebuilding; a
# checkout that has not moved costs a fingerprint check, not a compile. The
# target directory is pinned rather than inherited because the unit starts the
# binary from this exact path.
if ! CARGO_TARGET_DIR=$root/target CARGO_TERM_COLOR=never \
     "${CARGO:-cargo}" build --release -p thetis; then
    say "cargo failed; starting the binary that is already there"
    rm -f -- "$pinned"
    exit 0
fi

if [[ $(ident "$binary") == "$before" && -n $before ]]; then
    rm -f -- "$pinned"
    exit 0  # nothing to rebuild, and nothing worth a line in the journal
fi

# The same gate the gateway puts in front of a branch-built kernel before it
# adopts one: does this binary start and speak the current IPC protocol?
# INVOCATION_ID is dropped so the probe does not read itself as a supervised run.
if probe=$(env -u INVOCATION_ID -- "$binary" worker --probe 2>&1) &&
   [[ $probe == thetis-worker-probe-ok* ]]; then
    say "rebuilt from source and probed ok"
    rm -f -- "$pinned"
    exit 0
fi

say "the rebuilt binary did not pass the probe (${probe:-no output})"
if [[ -e $pinned ]] && mv -f -- "$pinned" "$binary"; then
    say "put the previous binary back; starting on that instead"
else
    say "no previous binary to fall back to; starting what was built"
fi
exit 0
