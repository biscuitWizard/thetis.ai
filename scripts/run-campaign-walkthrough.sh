#!/usr/bin/env bash
set -euo pipefail

# Start an isolated campaign gateway + scripted mock LLM, then run either the
# Playwright walkthrough (default) or the ignored Rust protocol test.
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mode=${1:---browser}
scratch=${THETIS_CAMPAIGN_SCRATCH:-$(mktemp -d /tmp/thetis-campaign.XXXXXX)}
artifacts=${THETIS_WALKTHROUGH_ARTIFACTS:-$scratch/artifacts}
mock_pid=
thetis_pid=
cleanup() {
  [[ -z ${thetis_pid:-} ]] || kill -- -"$thetis_pid" 2>/dev/null || true
  [[ -z ${mock_pid:-} ]] || kill -- -"$mock_pid" 2>/dev/null || true
  if [[ -z ${THETIS_CAMPAIGN_KEEP_SCRATCH:-} ]]; then rm -rf "$scratch"; else echo "scratch kept at $scratch"; fi
}
trap cleanup EXIT INT TERM
mkdir -p "$scratch"/{data,artifacts,worktrees,workspace}
# Run the orchestrators against a clean disposable checkout. Bootstrap artifacts
# are only cacheable for a clean tree; the developer checkout may contain this
# harness edit or unrelated work from another agent.
if [[ -d "$scratch/source/.git" ]]; then
  git -C "$scratch/source" add -A -- . ":!target*"
  git -C "$scratch/source" reset --hard "$(git -C "$root" rev-parse HEAD)" >/dev/null
else
  git clone --quiet --local --no-hardlinks "$root" "$scratch/source"
fi
# Include the working diff in the disposable checkout so review fixes are tested.
git -C "$root" diff --binary HEAD -- . ':!.claude/worktrees' ':!target*' > "$scratch/review.patch"
if [[ -s "$scratch/review.patch" ]]; then
  git -C "$scratch/source" apply "$scratch/review.patch"
  git -C "$scratch/source" add -A -- . ":!target*"
  git -C "$scratch/source" -c user.name='Campaign verification' -c user.email='verification@localhost' commit --quiet -m 'Snapshot campaign review changes'
fi
cp "$root/thetis.toml" "$scratch/thetis.toml"
# Distinct local model IDs make task routing observable without paid requests.
for role in architect plotting scene referee shop; do
  cat >>"$scratch/thetis.toml" <<EOF

[[models]]
id = "mock/$role"
label = "Test $role"
EOF
done
cat >"$scratch/thetis.local.toml" <<EOF
[server]
bind = "127.0.0.1:7797"
primary_gateway = "web"

[paths]
data = "$scratch/data"
artifacts = "$scratch/artifacts"
worktrees = "$scratch/worktrees"

[llm]
base_url = "http://127.0.0.1:7788"
model = "mock/echo"
api_key = "test"

[browser]
enabled = false

[discord]
enabled = false

[limits]
session_spend_limit_usd = 0.50
EOF

if [[ -z ${THETIS_CAMPAIGN_SKIP_BUILD:-} ]]; then
  cargo build --manifest-path "$root/Cargo.toml" -p thetis --bins
fi
# Exercise normal startup: mounted gateways must bootstrap without a prebuilt cache.
MOCK_LLM_SCRIPT="$root/services/playwright-sidecar/fixtures/campaign-walkthrough.json" \
  setsid "$root/target/debug/mock-llm" >"$scratch/mock-llm.log" 2>&1 &
mock_pid=$!
THETIS_ROOT="$scratch/source" THETIS_CONFIG="$scratch/thetis.toml" THETIS_LOCAL_CONFIG="" \
  THETIS_BIND="127.0.0.1:7797" THETIS_DATA_DIR="$scratch/data" THETIS_ARTIFACTS_DIR="$scratch/artifacts" \
  THETIS_WORKSPACE_DIR="$scratch/workspace" \
  setsid "$root/target/debug/thetis" >"$scratch/thetis.log" 2>&1 &
thetis_pid=$!

for _ in $(seq 1 180); do
  status=$(curl --silent --output /dev/null --write-out '%{http_code}' http://127.0.0.1:7797/play/ || true)
  if [[ "$status" == 200 ]] && curl --silent http://127.0.0.1:7797/play/ | rg -q 'id="play-area"'; then break; fi
  if ! kill -0 "$thetis_pid" 2>/dev/null; then
    cat "$scratch/thetis.log" >&2
    exit 1
  fi
  sleep 1
done
curl --fail --silent http://127.0.0.1:7797/play/ | rg -q 'id="play-area"' || {
  echo "scratch orchestrator did not become ready" >&2; cat "$scratch/thetis.log" >&2; exit 1;
}

case "$mode" in
  --serve)
    echo "Campaign review server ready at http://127.0.0.1:7797/play/"
    wait "$thetis_pid"
    ;;
  --protocol)
    THETIS_CAMPAIGN_WS_URL=ws://127.0.0.1:7797/play/ws \
      cargo test --manifest-path "$root/Cargo.toml" -p thetis --test ws_campaign -- --ignored --nocapture
    ;;
  --browser)
    THETIS_CAMPAIGN_URL=http://127.0.0.1:7797/play/ \
    THETIS_WALKTHROUGH_ARTIFACTS="$artifacts" \
      npm --prefix "$root/services/playwright-sidecar" run walkthrough:campaign
    for role in plotting scene referee shop; do
      rg -q "mock request model=mock/$role" "$scratch/mock-llm.log" || {
        echo "campaign task never used its selected $role model" >&2; exit 1;
      }
    done
    ;;
  *) echo "usage: $0 [--browser|--protocol]" >&2; exit 2 ;;
esac
