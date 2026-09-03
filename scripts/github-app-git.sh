#!/usr/bin/env bash
# Mints a GitHub App installation token and configures git to use it.
#
# The github-* tools cover commits, branches and PRs through the REST API, which
# needs no clone. This script is for the cases the API cannot do well: a real
# `git clone`, a rebase, a bisect, running a test suite against a working tree,
# or a commit series with exact parentage.
#
# It reads the same credentials the tools use — [tools.github] in
# thetis.local.toml or thetis.toml — and mints the same kind of installation
# token, using openssl for the RS256 signature.
#
# Usage:
#   eval "$(scripts/github-app-git.sh env)"     # export GITHUB_TOKEN, GIT_AUTHOR_*
#   scripts/github-app-git.sh token             # print just the token
#   scripts/github-app-git.sh clone owner/repo [dir]
#   scripts/github-app-git.sh identity          # show the [bot] author strings
#
# The token lasts one hour. Re-run when it expires.
set -euo pipefail

# The caller's directory is where a `clone` must land, so remember it before
# moving. Config lookup needs the project root, but silently cloning into the
# project instead of wherever the user ran from is a nasty surprise -- it drops
# a 100MB+ repo inside a git worktree, where it shows up as untracked cruft.
INVOKED_FROM="$PWD"
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_ROOT"

die() { echo "error: $*" >&2; exit 1; }

# --- Read config -------------------------------------------------------------
# The local file wins, matching how the orchestrator merges them.
# Search order mirrors the orchestrator's merge, most-specific-wins first. The
# shared overlay named by THETIS_LOCAL_CONFIG comes first because it is merged
# last and therefore wins; it also lives outside the worktree a conversation
# runs from, which is where a credential actually is.
config_files() {
  [ -n "${THETIS_LOCAL_CONFIG:-}" ] && echo "$THETIS_LOCAL_CONFIG"
  echo thetis.local.toml
  echo thetis.toml
}

read_key() {
  local key="$1" file
  while read -r file; do
    [ -n "$file" ] && [ -f "$file" ] || continue
    # The value from the [tools.github] section only: awk tracks the section so
    # a same-named key in another table cannot be picked up by accident.
    local found
    found=$(awk -v k="$key" '
      /^\[/ { in_section = ($0 ~ /^\[tools\.github\]/) ; next }
      in_section && $0 ~ "^[[:space:]]*"k"[[:space:]]*=" {
        sub(/^[^=]*=[[:space:]]*/, "")
        # Order matters: strip the trailing comment first, then the quotes.
        # Doing it the other way leaves the closing quote stranded before the
        # comment, and a stray quote in the app id makes the JWT claims
        # malformed JSON — which GitHub reports only as an opaque 401.
        sub(/[[:space:]]*#.*$/, "")
        sub(/[[:space:]]+$/, "")
        gsub(/^["'"'"']|["'"'"']$/, "")
        print; exit
      }
    ' "$file")
    [ -n "$found" ] && { echo "$found"; return 0; }
  done < <(config_files)
  return 0
}

APP_ID="${THETIS_TOOL_GITHUB_APP_ID:-$(read_key app_id)}"
KEY_PATH="${THETIS_TOOL_GITHUB_PRIVATE_KEY_PATH:-$(read_key private_key_path)}"
INSTALLATION_ID="${THETIS_TOOL_GITHUB_INSTALLATION_ID:-$(read_key installation_id)}"

[ -n "$APP_ID" ]   || die "no app_id found in [tools.github]. See thetis.toml for the block."
[ -n "$KEY_PATH" ] || die "no private_key_path found in [tools.github]."

# A relative key path is resolved against the project root *and* the shared
# overlay's directory, matching Config::secret_roots in the kernel. Without the
# second root, a key stored beside the shared overlay is invisible from the
# worktree a conversation runs in.
if [ ! -f "$KEY_PATH" ]; then
  for root in "" "$(dirname "${THETIS_LOCAL_CONFIG:-/nonexistent}")"; do
    [ -n "$root" ] && [ -f "$root/$KEY_PATH" ] && { KEY_PATH="$root/$KEY_PATH"; break; }
  done
fi
[ -f "$KEY_PATH" ] || die "private key not found at $KEY_PATH (looked in $PWD and beside \$THETIS_LOCAL_CONFIG)"

# --- Sign a JWT --------------------------------------------------------------
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

now=$(date +%s)
header=$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)
# iat is backdated a minute: GitHub rejects a token from a clock that runs fast,
# which otherwise shows up as a baffling intermittent 401.
# exp-iat must stay under GitHub's 10-minute ceiling. With iat backdated 60s,
# an exp of now+480 leaves 540s total — clear of the boundary either way.
claims=$(printf '{"iat":%d,"exp":%d,"iss":"%s"}' "$((now - 60))" "$((now + 480))" "$APP_ID" | b64url)
signature=$(printf '%s.%s' "$header" "$claims" \
  | openssl dgst -sha256 -sign "$KEY_PATH" \
  | b64url)
JWT="${header}.${claims}.${signature}"

api() {
  curl -sS --fail-with-body \
    -H "Authorization: Bearer $1" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    -H "User-Agent: thetis-grip" \
    "${@:2}"
}

# Same call without any Authorization header. `/users/{username}` is public and
# an App JWT is only valid on `/app/*`, so presenting one there earns a 401 that
# looks exactly like a broken private key.
api_anon() {
  curl -sS --fail-with-body \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    -H "User-Agent: thetis-grip" \
    "$@"
}

# --- Resolve the installation ------------------------------------------------
if [ -z "$INSTALLATION_ID" ]; then
  installations=$(api "$JWT" https://api.github.com/app/installations)
  count=$(echo "$installations" | jq 'length')
  case "$count" in
    0) die "the App has no installations. Install it on an account or org first." ;;
    1) INSTALLATION_ID=$(echo "$installations" | jq -r '.[0].id') ;;
    *) echo "$installations" | jq -r '.[] | "  \(.id)  \(.account.login)"' >&2
       die "the App has $count installations; set installation_id in [tools.github]." ;;
  esac
fi

TOKEN=$(api "$JWT" -X POST \
  "https://api.github.com/app/installations/${INSTALLATION_ID}/access_tokens" \
  | jq -r '.token')
[ -n "$TOKEN" ] && [ "$TOKEN" != "null" ] || die "could not mint an installation token"

# --- The bot identity --------------------------------------------------------
# GitHub links a commit to the App only when the committer email matches
# `<bot-user-id>+<slug>[bot]@users.noreply.github.com`, and the id is the *bot
# user's*, not the App's. Getting this wrong is the usual reason an App-authored
# commit shows up as an unlinked stranger.
SLUG=$(api "$JWT" https://api.github.com/app | jq -r '.slug')
BOT_ID=$(api_anon "https://api.github.com/users/${SLUG}%5Bbot%5D" | jq -r '.id')
BOT_NAME="${SLUG}[bot]"
BOT_EMAIL="${BOT_ID}+${SLUG}[bot]@users.noreply.github.com"

case "${1:-env}" in
  token)
    echo "$TOKEN"
    ;;
  identity)
    echo "name:  $BOT_NAME"
    echo "email: $BOT_EMAIL"
    echo "installation: $INSTALLATION_ID"
    ;;
  env)
    # `x-access-token` is the username GitHub expects for an installation token.
    echo "export GITHUB_TOKEN='$TOKEN'"
    echo "export GIT_AUTHOR_NAME='$BOT_NAME'"
    echo "export GIT_AUTHOR_EMAIL='$BOT_EMAIL'"
    echo "export GIT_COMMITTER_NAME='$BOT_NAME'"
    echo "export GIT_COMMITTER_EMAIL='$BOT_EMAIL'"
    # Rewrites any github.com URL to carry the token, so an existing remote and
    # a fresh clone both authenticate without the token appearing in .git/config.
    echo "export GIT_CONFIG_COUNT=1"
    echo "export GIT_CONFIG_KEY_0='url.https://x-access-token:${TOKEN}@github.com/.insteadOf'"
    echo "export GIT_CONFIG_VALUE_0='https://github.com/'"
    ;;
  clone)
    repo="${2:?usage: $0 clone owner/repo [dir]}"
    repo="${repo#https://github.com/}"; repo="${repo%.git}"
    # Back to where the user actually invoked us, so a relative target means
    # what they meant by it.
    cd "$INVOKED_FROM"
    target="${3:-$(basename "$repo")}"
    git clone "https://x-access-token:${TOKEN}@github.com/${repo}.git" ${3:+"$3"}
    # Scrub the token out of the remote URL. `git clone` persists whatever URL
    # it was given into .git/config, so leaving it there would write an hour-long
    # credential to disk in plain text -- and it expires anyway, so a later push
    # would fail with a stale secret rather than re-authenticating. Callers use
    # `eval "$(... env)"`, whose insteadOf rewrite supplies a fresh token.
    git -C "$target" remote set-url origin "https://github.com/${repo}.git"
    git -C "$target" config user.name "$BOT_NAME"
    git -C "$target" config user.email "$BOT_EMAIL"
    echo "cloned $repo into $target, committing as $BOT_NAME"
    echo "remote is tokenless; run: eval \"\$($0 env)\"  before pushing"
    ;;
  *)
    die "unknown command ${1}. Try: env, token, identity, clone"
    ;;
esac
