#!/usr/bin/env bash
set -euo pipefail

# Start an isolated campaign gateway + scripted mock LLM, then run either the
# Playwright walkthrough (default) or the ignored Rust protocol test.
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mode=${1:---browser}
scratch=${THETIS_CAMPAIGN_SCRATCH:-$(mktemp -d /tmp/thetis-campaign.XXXXXX)}
artifacts=${THETIS_WALKTHROUGH_ARTIFACTS:-$scratch/artifacts}
mock_pid=
bootstrap_pid=
thetis_pid=
cleanup() {
  [[ -z ${thetis_pid:-} ]] || kill -- -"$thetis_pid" 2>/dev/null || true
  [[ -z ${bootstrap_pid:-} ]] || kill -- -"$bootstrap_pid" 2>/dev/null || true
  [[ -z ${mock_pid:-} ]] || kill -- -"$mock_pid" 2>/dev/null || true
  if [[ -z ${THETIS_CAMPAIGN_KEEP_SCRATCH:-} ]]; then rm -rf "$scratch"; else echo "scratch kept at $scratch"; fi
}
trap cleanup EXIT INT TERM
mkdir -p "$scratch"/{data,artifacts,worktrees}
# Run the orchestrators against a clean disposable checkout. Bootstrap artifacts
# are only cacheable for a clean tree; the developer checkout may contain this
# harness edit or unrelated work from another agent.
git clone --quiet --local --no-hardlinks "$root" "$scratch/source"
cp "$root/thetis.toml" "$scratch/thetis.toml"
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
# A fresh gateway bootstraps only its primary UI. Briefly make campaign primary
# in a separate scratch process to compile, smoke-test, and cache that artifact;
# the real walkthrough process remains gateway-web with campaign mounted at
# /play. Both use only the disposable data/artifact/config directories.
cat >"$scratch/bootstrap.toml" <<EOF
[server]
bind = "127.0.0.1:7798"
primary_gateway = "campaign"

[paths]
data = "$scratch/data-bootstrap"
artifacts = "$scratch/artifacts"
worktrees = "$scratch/worktrees-bootstrap"

[browser]
enabled = false

[discord]
enabled = false
EOF
mkdir -p "$scratch/data-bootstrap" "$scratch/worktrees-bootstrap"
THETIS_ROOT="$scratch/source" THETIS_CONFIG="$scratch/bootstrap.toml" THETIS_LOCAL_CONFIG="" \
  THETIS_BIND="127.0.0.1:7798" THETIS_DATA_DIR="$scratch/data-bootstrap" \
  THETIS_ARTIFACTS_DIR="$scratch/artifacts" THETIS_WORKTREES_DIR="$scratch/worktrees-bootstrap" \
  setsid "$root/target/debug/thetis" >"$scratch/bootstrap.log" 2>&1 &
bootstrap_pid=$!
for _ in $(seq 1 180); do
  if grep -Eq '(UI bootstrapped and serving|serving trunk.s UI from the build cache).*gateway/campaign' "$scratch/bootstrap.log"; then break; fi
  if ! kill -0 "$bootstrap_pid" 2>/dev/null; then
    cat "$scratch/bootstrap.log" >&2
    exit 1
  fi
  sleep 1
done
if ! grep -Eq '(UI bootstrapped and serving|serving trunk.s UI from the build cache).*gateway/campaign' "$scratch/bootstrap.log"; then
  echo "campaign gateway bootstrap did not finish" >&2
  cat "$scratch/bootstrap.log" >&2
  exit 1
fi
kill -- -"$bootstrap_pid" 2>/dev/null || true
wait "$bootstrap_pid" 2>/dev/null || true
bootstrap_pid=

MOCK_LLM_SCRIPT="$root/services/playwright-sidecar/fixtures/campaign-walkthrough.json" \
  setsid "$root/target/debug/mock-llm" >"$scratch/mock-llm.log" 2>&1 &
mock_pid=$!
THETIS_ROOT="$scratch/source" THETIS_CONFIG="$scratch/thetis.toml" THETIS_LOCAL_CONFIG="" \
  THETIS_BIND="127.0.0.1:7797" THETIS_DATA_DIR="$scratch/data" THETIS_ARTIFACTS_DIR="$scratch/artifacts" \
  THETIS_WORKSPACE_DIR="${THETIS_WORKSPACE_DIR:-/opt/thetis/workspace}" \
  setsid "$root/target/debug/thetis" >"$scratch/thetis.log" 2>&1 &
thetis_pid=$!

for _ in $(seq 1 180); do
  status=$(curl --silent --output /dev/null --write-out '%{http_code}' http://127.0.0.1:7797/play/ || true)
  if [[ "$status" == 200 ]]; then break; fi
  if ! kill -0 "$thetis_pid" 2>/dev/null; then
    cat "$scratch/thetis.log" >&2
    exit 1
  fi
  sleep 1
done
curl --fail --silent http://127.0.0.1:7797/play/ >/dev/null || {
  echo "scratch orchestrator did not become ready" >&2; cat "$scratch/thetis.log" >&2; exit 1;
}

case "$mode" in
  --protocol)
    THETIS_CAMPAIGN_WS_URL=ws://127.0.0.1:7797/play/ws \
      cargo test --manifest-path "$root/Cargo.toml" -p thetis --test ws_campaign -- --ignored --nocapture
    ;;
  --browser)
    THETIS_CAMPAIGN_URL=http://127.0.0.1:7797/play/ \
    THETIS_WALKTHROUGH_ARTIFACTS="$artifacts" \
      npm --prefix "$root/services/playwright-sidecar" run walkthrough:campaign
    ;;
  *) echo "usage: $0 [--browser|--protocol]" >&2; exit 2 ;;
esac
