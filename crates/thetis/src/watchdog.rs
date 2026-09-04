//! Liveness probes and circuit breakers.
//!
//! The epoch budget in `runtime` stops a single call from running away. This is
//! the layer above: it notices when a *revision* is consistently failing and
//! takes it out of service automatically, so a bad self-modification degrades
//! into a rollback rather than an agent nobody can reach.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::aspect::Aspect;
use crate::grip::Grip;
use crate::pipeline;

pub struct Breakers {
    /// aspect key -> failure timestamps, newest last
    failures: Mutex<HashMap<String, Vec<Instant>>>,
    window: Duration,
    threshold: usize,
}

impl Breakers {
    pub fn new(window: Duration, threshold: usize) -> Self {
        Self {
            failures: Mutex::new(HashMap::new()),
            window,
            threshold,
        }
    }

    /// Records a failure and reports whether the breaker has tripped.
    pub fn record_failure(&self, aspect: &Aspect) -> bool {
        let Ok(mut map) = self.failures.lock() else {
            return false;
        };
        let entry = map.entry(aspect.key()).or_default();
        let now = Instant::now();
        entry.retain(|t| now.duration_since(*t) < self.window);
        entry.push(now);
        entry.len() >= self.threshold
    }

    /// Called after a healthy result, and after a rollback, so a recovered aspect
    /// starts from a clean slate.
    pub fn clear(&self, aspect: &Aspect) {
        if let Ok(mut map) = self.failures.lock() {
            map.remove(&aspect.key());
        }
    }

    pub fn failure_count(&self, aspect: &Aspect) -> usize {
        self.failures
            .lock()
            .ok()
            .and_then(|m| m.get(&aspect.key()).map(Vec::len))
            .unwrap_or(0)
    }
}

/// Reports a failed guest call to the breaker, rolling the aspect back if it has
/// failed too often. Returns a message when a rollback happened.
pub async fn report_failure(grip: &Arc<Grip>, aspect: &Aspect, detail: &str) -> Option<String> {
    if !grip.breakers.record_failure(aspect) {
        tracing::debug!(%aspect, detail, "guest call failed");
        return None;
    }

    tracing::warn!(
        %aspect,
        failures = grip.breakers.failure_count(aspect),
        "circuit breaker tripped; rolling back"
    );

    match pipeline::reset_aspect_to_green(grip, aspect).await {
        Ok(message) => {
            grip.breakers.clear(aspect);
            let text = format!("{aspect} kept failing ({detail}); {message}");
            tracing::warn!("{text}");
            Some(text)
        }
        Err(e) => {
            // Nothing to fall back to. Say so loudly rather than pretending.
            let text = format!("{aspect} kept failing ({detail}) and could not be reset: {e:#}");
            tracing::error!("{text}");
            Some(text)
        }
    }
}

/// Periodically probes the active agent so a version that only fails at runtime
/// is discovered before a user runs into it.
pub fn spawn_prober(grip: Arc<Grip>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(grip.cfg().watchdog.probe_interval);
        // Default `Burst` behaviour replays every tick missed while a probe (or
        // a rollback) was slow, back to back. That turned "three failures in
        // two minutes" — the breaker's whole meaning — into three failures in
        // a millisecond, tripping it on one slow moment.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick fires immediately; skip it

        loop {
            ticker.tick().await;
            if let Err(e) = probe_agent(&grip).await {
                report_failure(&grip, &Aspect::Agent, &format!("health probe: {e:#}")).await;
            } else {
                grip.breakers.clear(&Aspect::Agent);
            }
        }
    });
}

async fn probe_agent(grip: &Arc<Grip>) -> anyhow::Result<()> {
    use crate::runtime::{Budget, Caps};

    let Some(loaded) = grip.loader.get(&Aspect::Agent) else {
        return Ok(()); // nothing loaded yet; not a failure
    };

    let budget = Budget::probe("agent health probe", grip.cfg().probe_budget);
    let mut store = grip
        .runtime
        .new_store(grip.clone(), Caps::Agent, budget, None);

    let agent = crate::bindings::agent::Agent::instantiate_async(
        &mut store,
        &loaded.component,
        grip.runtime.linker(Caps::Agent),
    )
    .await
    .map_err(anyhow::Error::from)?;

    let health = agent
        .call_health(&mut store)
        .await
        .map_err(anyhow::Error::from)?;

    if health.trim().is_empty() {
        anyhow::bail!("health returned nothing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breakers() -> Breakers {
        Breakers::new(Duration::from_secs(120), 3)
    }

    #[test]
    fn breaker_trips_only_after_repeated_failures() {
        let b = breakers();
        let aspect = Aspect::Agent;

        assert!(!b.record_failure(&aspect), "one failure is not a pattern");
        assert!(!b.record_failure(&aspect));
        assert!(b.record_failure(&aspect), "third failure trips it");
    }

    #[test]
    fn breakers_are_per_aspect() {
        let b = breakers();
        b.record_failure(&Aspect::Agent);
        b.record_failure(&Aspect::Agent);

        // A different aspect must not inherit the agent's failures.
        assert!(!b.record_failure(&Aspect::gateway("web")));
        assert_eq!(b.failure_count(&Aspect::Agent), 2);
        assert_eq!(b.failure_count(&Aspect::gateway("web")), 1);
    }

    #[test]
    fn recovery_clears_the_breaker() {
        let b = breakers();
        let aspect = Aspect::tool("flaky");

        b.record_failure(&aspect);
        b.record_failure(&aspect);
        b.clear(&aspect);

        assert_eq!(b.failure_count(&aspect), 0);
        assert!(
            !b.record_failure(&aspect),
            "counting restarts after recovery"
        );
    }
}
