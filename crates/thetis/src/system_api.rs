//! The `system-status` frame: what the whole installation is doing right now.
//!
//! Answered host-side, like `branch_api` and `debug_api`, because everything in
//! it is out of the gateway guest's reach: trunk's git refs, the loader's
//! notion of which UI build is being served, the worker fleet, and the machine
//! itself.
//!
//! Two rules shape it. It never spawns a worker — the fleet is asked through
//! `live_peer` only, so opening a browser tab cannot resurrect conversations.
//! And it is cheap enough to poll: one `git log -1`, one `lookup` in the build
//! cache, three small reads under `/proc`, and one short IPC call per live
//! worker, all bounded by a timeout so a wedged worker degrades to "unknown"
//! instead of hanging the frame.

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

use crate::grip::{Grip, Role};

/// How long a worker gets to answer `health` before it is counted unknown.
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);

/// When this process started, for the uptime figure. Set by the gateway at
/// boot; unset (and so reported as null) in any other role.
static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Records this process's start time. Called once, from the gateway's `run`.
pub fn mark_start() {
    let _ = STARTED.set(std::time::Instant::now());
}

/// True when this frame type belongs here.
pub fn handles(frame_type: &str) -> bool {
    frame_type == "system-status"
}

/// Handles one frame, returning the reply frames to send on this socket.
pub async fn handle(
    grip: &Arc<Grip>,
    principal: &crate::auth::Principal,
    _frame: &Value,
) -> Vec<String> {
    vec![status(grip, principal).await.to_string()]
}

async fn status(grip: &Arc<Grip>, principal: &crate::auth::Principal) -> Value {
    let trunk = trunk_facts(grip).await;
    let ui = ui_facts(grip).await;
    let fleet = fleet_facts(grip).await;

    // One word for the whole system, chosen by what is most worth knowing:
    // a build in flight outranks a turn in flight, which outranks idling, and
    // a UI artifact that no longer matches trunk outranks both because it is
    // the one state a restart fixes.
    let state = if fleet.building > 0 {
        "building"
    } else if fleet.turning > 0 {
        "working"
    } else if ui.serving == "fallback" {
        "degraded"
    } else if ui.serving == "stale" {
        "stale"
    } else {
        "running"
    };

    json!({
        "type": "system-status",
        "ok": true,
        "state": state,
        "version": env!("CARGO_PKG_VERSION"),
        "wit": crate::pipeline::kernel_wit_fingerprint(),
        "uptime_s": STARTED.get().map(|t| t.elapsed().as_secs()),
        // The asker's own conversations, or everyone's when this connection
        // is showing everyone's: the count should match the sidebar.
        "sessions": grip
            .persist
            .list_sessions_owned(principal.list_owner(), false)
            .await
            .map(|s| s.len())
            .unwrap_or(0),
        "trunk": trunk,
        "ui": {
            "aspect": ui.aspect,
            "revision": ui.revision,
            "serving": ui.serving,
        },
        "workers": {
            "live": fleet.live,
            "building": fleet.building,
            "turning": fleet.turning,
            "unknown": fleet.unknown,
            "rss_kb": fleet.rss_kb,
        },
        "host": host_facts(),
    })
}

/// Trunk's name, head commit and cleanliness — the version everything else
/// starts from and the page is served from.
async fn trunk_facts(grip: &Arc<Grip>) -> Value {
    let root = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
    let name = root
        .current_branch()
        .await
        .unwrap_or_else(|_| "trunk".to_string());
    let head = root.log("HEAD", 1).await.unwrap_or_default();
    let head = head.first();
    json!({
        "name": name,
        "rev": head.map(|c| c.rev.clone()).unwrap_or_default(),
        "subject": head.map(|c| c.subject.clone()).unwrap_or_default(),
        "author": head.map(|c| c.author.clone()).unwrap_or_default(),
        "ts_ms": head.map(|c| c.ts_ms),
        "dirty": root.is_dirty().await.unwrap_or(false),
    })
}

struct UiFacts {
    aspect: String,
    revision: Option<u64>,
    /// `current` (the loaded artifact is trunk's), `stale` (trunk moved on),
    /// `fallback` (nothing loaded, the host-rendered page is showing), or
    /// `unknown` (no cache key — an untracked or non-git checkout).
    serving: &'static str,
}

async fn ui_facts(grip: &Arc<Grip>) -> UiFacts {
    let aspect = crate::aspect::Aspect::gateway(&grip.cfg.primary_gateway);
    let loaded = grip.loader.get(&aspect);
    let revision = loaded.as_ref().map(|c| c.revision);

    let trunk = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
    let key = crate::pipeline::cache_key_with(&trunk, &grip.cfg, "HEAD", &aspect).await;

    let serving = match (revision, key) {
        (None, _) => "fallback",
        (Some(_), None) => "unknown",
        (Some(rev), Some(key)) => {
            if rev == crate::pipeline::key_revision(&key) {
                "current"
            } else {
                "stale"
            }
        }
    };

    UiFacts {
        aspect: aspect.key(),
        revision,
        serving,
    }
}

#[derive(Default)]
struct FleetFacts {
    live: usize,
    /// Busy with something that is not a turn: a build.
    building: usize,
    turning: usize,
    unknown: usize,
    /// Resident memory across every live worker, in KiB.
    rss_kb: u64,
}

/// Asks each live worker how it is. Never materializes one: a conversation
/// with no worker is simply not counted.
async fn fleet_facts(grip: &Arc<Grip>) -> FleetFacts {
    let Role::Gateway(router) = &grip.role else {
        return FleetFacts::default();
    };

    let mut facts = FleetFacts::default();
    for session in router.live_sessions().await {
        let Some(peer) = router.live_peer(&session).await else {
            continue;
        };
        facts.live += 1;
        let reply = tokio::time::timeout(HEALTH_TIMEOUT, peer.call("health", json!({}))).await;
        match reply {
            Ok(Ok(health)) => {
                let turn = health.get("turn").and_then(Value::as_bool).unwrap_or(false);
                let busy = health.get("busy").and_then(Value::as_bool).unwrap_or(false);
                if turn {
                    facts.turning += 1;
                } else if busy {
                    facts.building += 1;
                }
                facts.rss_kb += health.get("rss_kb").and_then(Value::as_u64).unwrap_or(0);
            }
            // A worker too busy or too old to answer is reported as such
            // rather than silently counted as healthy.
            _ => facts.unknown += 1,
        }
    }
    facts
}

/// The machine: memory, load, cores, and this process's own footprint.
///
/// Every field is optional. These are Linux `/proc` reads, and a platform
/// without them should show a shorter toolbar, not an error.
pub fn host_facts() -> Value {
    let mem = meminfo();
    json!({
        "mem_total_kb": mem.0,
        "mem_available_kb": mem.1,
        "load1": loadavg(),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "rss_kb": self_rss_kb(),
    })
}

/// `MemTotal` and `MemAvailable`, in KiB. `MemAvailable` is the honest figure
/// for "how much more could run here" — `MemFree` counts none of the
/// reclaimable page cache and reads alarmingly low on a healthy machine.
fn meminfo() -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let field = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<u64>().ok())
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

fn loadavg() -> Option<f64> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// This process's resident set size in KiB. Public because the worker reports
/// its own through the `health` call.
pub fn self_rss_kb() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_type_routing_is_exact() {
        assert!(handles("system-status"));
        // Nothing else may be swallowed here: an unrouted frame has to reach
        // the guest, which is what produces "unknown frame type".
        assert!(!handles("system"));
        assert!(!handles("system-status-extra"));
        assert!(!handles("branch-status"));
    }

    /// The bar's memory meter divides by MemTotal and reads MemAvailable for
    /// headroom. A parse that silently returned None would draw no meter at
    /// all, so this pins the two field names and the KiB units.
    #[test]
    fn host_facts_report_real_memory_and_cpus() {
        let facts = host_facts();
        let total = facts["mem_total_kb"].as_u64().expect("MemTotal");
        let available = facts["mem_available_kb"].as_u64().expect("MemAvailable");
        assert!(total > 0, "MemTotal parsed as zero");
        assert!(
            available <= total,
            "available {available} exceeds total {total}"
        );
        assert!(facts["cpus"].as_u64().unwrap_or(0) >= 1);
        assert!(facts["load1"].as_f64().is_some(), "loadavg did not parse");
        assert!(self_rss_kb().unwrap_or(0) > 0, "VmRSS did not parse");
    }
}
