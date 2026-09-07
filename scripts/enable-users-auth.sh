#!/usr/bin/env bash
#
# Switches Thetis from the single implicit administrator to real accounts.
#
#   scripts/enable-users-auth.sh <user-id> ["Display Name"]
#
# The password is read from the terminal, never from an argument or the
# environment: an argument is world-readable in /proc and lands in shell
# history, and neither is a thing to do with a password that guards every
# conversation on the machine. Only its Argon2 hash is written anywhere.
#
# Everything lands in thetis.local.toml, which is gitignored. The tracked
# thetis.toml keeps shipping `mode = "local"`, so a clone of this repo is still
# a single-user system until somebody runs this.
#
# The config is checked before the old one is let go, and restored if the new
# one will not load. A half-written [[users]] block is otherwise discovered by
# restarting and finding Thetis gone.
set -uo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd) || exit 1
overlay=$root/thetis.local.toml
binary=$root/target/release/thetis

die() { echo "enable-users-auth: $*" >&2; exit 1; }

given=${1:-}
[ -n "$given" ] || die "usage: scripts/enable-users-auth.sh <user-id> [\"Display Name\"]"

# The display name keeps whatever capitalisation was typed; the id is
# lowercased to match what the configuration accepts. Signing in is
# case-insensitive, so the account is still reachable by the name as written.
name=${2:-$given}
id=$(printf '%s' "$given" | tr '[:upper:]' '[:lower:]')

# Exactly the rule config.rs enforces. Checked here, before the password is
# asked for: validating after meant typing a password twice, having it hashed
# and written, and only then being told the id was never going to be accepted.
[ ${#id} -le 64 ] || die "a user id may be at most 64 characters"
[[ $id =~ ^[a-z0-9._-]+$ ]] \
    || die "a user id may hold only lowercase letters, digits, dot, underscore and hyphen — \`$given\` is not one"
[ "$id" = "$given" ] || echo "enable-users-auth: using id \`$id\` (display name stays \`$name\`)" >&2

[ -x "$binary" ] || die "no binary at $binary — run: cargo build --release -p thetis"

if [ -f "$overlay" ] && grep -qE '^\[\[users\]\]' "$overlay"; then
    die "$overlay already defines [[users]]; edit it by hand rather than adding a second account here"
fi

# Read twice, silently. `read -s` keeps it off the screen; the shell never sees
# it as a word, so it cannot reach history or the process table.
[ -t 0 ] || die "run this from a terminal: it has to prompt for a password"
printf 'Password for %s: ' "$id" >&2
read -rs password; echo >&2
printf 'Again: ' >&2
read -rs confirm; echo >&2
[ -n "$password" ] || die "an empty password is not a password"
[ "$password" = "$confirm" ] || die "the two did not match"

hash=$(printf '%s' "$password" | "$binary" hash-password --stdin 2>/dev/null) \
    || die "hashing failed"
unset password confirm
[ -n "$hash" ] || die "hashing produced nothing"

backup=$overlay.before-users-auth
[ -f "$overlay" ] && cp -- "$overlay" "$backup"
touch -- "$overlay"

cat >> "$overlay" <<TOML

# --- accounts -----------------------------------------------------------
# Written by scripts/enable-users-auth.sh. Passwords are Argon2 hashes; the
# password itself was never stored. Conversations owned by the "local"
# placeholder are re-owned to claim_unowned on the next start, so the history
# from before this switch stays reachable.
[auth]
mode = "users"
claim_unowned = "$id"

[[roles]]
id = "admin"
admin = true

[[users]]
id = "$id"
name = "$name"
role = "admin"
password_hash = "$hash"
TOML

if ! out=$("$binary" check-config 2>&1); then
    if [ -f "$backup" ]; then mv -- "$backup" "$overlay"; else rm -f -- "$overlay"; fi
    echo "$out" >&2
    die "the new configuration does not load; nothing was changed"
fi

rm -f -- "$backup"
echo "$out" | grep -v '^20[0-9][0-9]-'

# The bind above is what the *files* say. A systemd EnvironmentFile can
# override it and is not readable from here, so ask the running process
# instead: users mode off loopback needs server.public_origin, and finding that
# out from a unit that will not start is the wrong way to find it out.
live=$(ss -ltnp 2>/dev/null | grep -F "\"thetis\"" | awk '{print $4}' | head -1)
if [ -n "$live" ]; then
    case ${live%:*} in
        127.*|::1|\[::1\]) ;;
        *)
            grep -qE '^[[:space:]]*public_origin[[:space:]]*=' "$overlay" "$root/thetis.toml" \
                || echo "enable-users-auth: warning — thetis is listening on $live, which is not" \
                        "loopback. Users mode needs server.public_origin set before it will" \
                        "start. Set it, or Thetis will refuse to come back up." >&2
            ;;
    esac
fi
cat >&2 <<DONE

Written to $overlay (gitignored).
Restart to apply it:  systemctl restart thetis
Then sign in as "$id" at the address above.
DONE
