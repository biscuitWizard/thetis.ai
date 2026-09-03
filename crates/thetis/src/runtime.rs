//! Wasmtime runtime plumbing: the engine, per-call stores, capability-scoped
//! linkers, and the epoch budget that keeps a misbehaving guest from wedging
//! the process.
//!
//! Every guest call gets a *fresh* store. That is what makes hot swapping safe:
//! no guest state survives a call, so swapping the component between calls can
//! never leave a half-migrated instance behind.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wasmtime::component::{HasSelf, Linker, ResourceTable};
use wasmtime::{
    Config as WasmConfig, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline,
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};

use crate::bindings;
use crate::config::Config;
use crate::grip::Grip;
use crate::llm::StreamHandle;

/// How often the epoch counter advances. The deadline callback runs at most
/// this often, which bounds how long a runaway guest can spin undetected.
pub const EPOCH_TICK: Duration = Duration::from_millis(100);
/// Ticks granted between deadline-callback checks.
const TICKS_PER_CHECK: u64 = 1;

/// Which host capabilities a guest is allowed to link against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caps {
    /// The agent: everything.
    Agent,
    /// Gateways: session management and sys only. No LLM, no exec, no devkit.
    Gateway,
    /// Tools: sys and the sandbox. No LLM, no session log, no self-modification.
    Tool,
}

/// Wall-clock and CPU limits for a single guest call.
pub struct Budget {
    /// What this call is, for the trap message.
    pub label: String,
    pub started: Instant,
    /// Refreshed whenever the guest is seen to have called the host since the
    /// last check, so time spent *inside* the host is not charged as guest CPU.
    pub last_yield: Instant,
    /// Host imports entered so far, and the count as of the last epoch check.
    ///
    /// This is what makes the spin clock honest. It used to be refreshed by an
    /// explicit `yielded()` at the end of each import, and three whole
    /// interface blocks — `sys`, `session`, `hostfs` — never called it, so a
    /// slow `events` scan or a large `search_files` was billed to the guest and
    /// a healthy turn trapped as "likely an infinite loop" (which also counts
    /// toward the watchdog rolling that aspect back). Counting entries instead
    /// means a new import cannot forget: the epoch callback sees the count
    /// move and resets the clock.
    pub host_calls: u64,
    seen_calls: u64,
    /// The last import the guest entered, named in the trap message so a
    /// wrongly-blamed turn is diagnosable rather than a mystery.
    pub last_import: &'static str,
    /// Longest the guest may run without returning to a blocking host import.
    ///
    /// This is the only time limit. There is deliberately no wall-clock ceiling
    /// on a call: a turn that streams a long answer, runs a dozen tools and
    /// compiles something is doing exactly what it should, and killing it at an
    /// arbitrary number of seconds destroys real work while catching nothing a
    /// runaway would not also trip here. What actually distinguishes a wedged
    /// guest is that it stops talking to the host, which is what this measures.
    pub slice: Duration,
    /// Set when the user stops the turn. Not a violation on its own: see
    /// [`Budget::cancelled`].
    cancel_requested: Option<Instant>,
}

/// How long a stopped guest is given to return from `handle-turn` of its own
/// accord before it is trapped.
///
/// A trap works, but it costs: the turn ends as an incident rather than a clean
/// stop, and the agent's own "cancelled" bookkeeping never runs. A cooperative
/// guest notices the cancel at its next checkpoint and returns within
/// microseconds, so this window is almost never spent — it exists so that a
/// guest which ignores the signal still cannot keep running.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

impl Budget {
    pub fn new(label: impl Into<String>, slice: Duration) -> Self {
        let now = Instant::now();
        Self {
            label: label.into(),
            started: now,
            last_yield: now,
            host_calls: 0,
            seen_calls: 0,
            last_import: "-",
            slice,
            cancel_requested: None,
        }
    }

    /// A short call that is not expected to yield at all, such as `health()`.
    pub fn probe(label: impl Into<String>, limit: Duration) -> Self {
        Self::new(label, limit)
    }

    /// Records that the guest just came back from a blocking host call.
    pub fn yielded(&mut self) {
        self.last_yield = Instant::now();
    }

    /// Records that the guest has entered a host import. Called on the way
    /// *in*, so it cannot be skipped by a `?` on the way out.
    pub fn entered_host(&mut self, import: &'static str) {
        self.host_calls = self.host_calls.wrapping_add(1);
        self.last_import = import;
    }

    /// Called at every epoch check, before judging. If the guest has been in
    /// the host since the last check it was not spinning, whatever the clock
    /// says.
    fn checkpoint(&mut self) {
        if self.host_calls != self.seen_calls {
            self.seen_calls = self.host_calls;
            self.last_yield = Instant::now();
        }
    }

    /// Asks the guest to stop, starting its grace window.
    ///
    /// Idempotent: a user who presses stop repeatedly must not keep pushing the
    /// deadline out, so the first request is the one that counts.
    pub fn cancel(&mut self) {
        if self.cancel_requested.is_none() {
            self.cancel_requested = Some(Instant::now());
        }
    }

    /// Whether a stop has been requested for this call.
    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested.is_some()
    }

    fn violation(&self) -> Option<String> {
        // Only once the grace window has run out. Trapping the instant the stop
        // arrives would deny a cooperative guest the chance to return cleanly,
        // which is the difference between a turn that ends as "stopped" and one
        // that ends as a crash the watchdog might roll the agent back for.
        if let Some(at) = self.cancel_requested {
            if at.elapsed() > CANCEL_GRACE {
                return Some(format!(
                    "{}: stopped by the user, and did not return within {CANCEL_GRACE:?}",
                    self.label
                ));
            }
            // Still within the window: do not charge the spin timer either, or
            // a guest winding down would trip that instead.
            return None;
        }
        let spinning = self.last_yield.elapsed();
        if spinning > self.slice {
            return Some(format!(
                "{}: ran {:?} without yielding to a host call (limit {:?}) — likely an infinite loop \
                 (last host import: {})",
                self.label, spinning, self.slice, self.last_import
            ));
        }
        None
    }
}

pub struct HostState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    /// The crate's no-op hook set; we do not customise wasi:http behaviour.
    http_hooks: [(); 0],
    table: ResourceTable,
    limits: StoreLimits,
    pub grip: Arc<Grip>,
    pub budget: Budget,
    /// Session this call acts on behalf of. Imports use it to scope access so a
    /// guest cannot reach into a session it was not invoked for.
    pub session_id: Option<String>,
    pub principal: Option<Arc<crate::auth::Principal>>,
    pub policy: Arc<crate::policy::EffectivePolicy>,
    pub streams: HashMap<u64, StreamHandle>,
    pub next_stream_id: u64,
    /// Aspects the guest asked to swap at the end of this call (self-modification
    /// never yanks a running instance).
    pub pending_swaps: Vec<crate::aspect::Aspect>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl HostState {
    /// Marks the end of a blocking host call so waiting is not charged as spin.
    pub fn yielded(&mut self) {
        self.budget.yielded();
    }
}

pub struct Runtime {
    pub engine: Engine,
    pub agent_linker: Linker<HostState>,
    pub gateway_linker: Linker<HostState>,
    pub tool_linker: Linker<HostState>,
    cfg: Arc<Config>,
}

impl Runtime {
    pub fn new(cfg: Arc<Config>) -> Result<Arc<Self>> {
        let mut wasm_cfg = WasmConfig::new();
        wasm_cfg.epoch_interruption(true);
        wasm_cfg.wasm_component_model(true);
        let engine = Engine::new(&wasm_cfg)
            .map_err(anyhow::Error::from)
            .context("creating wasmtime engine")?;

        // A single ticker drives deadlines for every store in the process.
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("thetis-epoch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(EPOCH_TICK);
                    ticker.increment_epoch();
                }
            })
            .context("spawning epoch ticker")?;

        let agent_linker = build_linker(&engine, Caps::Agent)?;
        let gateway_linker = build_linker(&engine, Caps::Gateway)?;
        let tool_linker = build_linker(&engine, Caps::Tool)?;

        Ok(Arc::new(Self {
            engine,
            agent_linker,
            gateway_linker,
            tool_linker,
            cfg,
        }))
    }

    pub fn linker(&self, caps: Caps) -> &Linker<HostState> {
        match caps {
            Caps::Agent => &self.agent_linker,
            Caps::Gateway => &self.gateway_linker,
            Caps::Tool => &self.tool_linker,
        }
    }

    /// Builds a fresh store for one guest call.
    pub fn new_store(
        &self,
        grip: Arc<Grip>,
        caps: Caps,
        budget: Budget,
        session_id: Option<String>,
    ) -> Store<HostState> {
        let memory_cap = match caps {
            Caps::Agent => self.cfg.agent_memory_bytes,
            Caps::Gateway => self.cfg.gateway_memory_bytes,
            Caps::Tool => self.cfg.tool_memory_bytes,
        };

        let state = HostState {
            wasi: self.wasi_ctx(),
            http: WasiHttpCtx::new(),
            http_hooks: [],
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_cap)
                .instances(8)
                .tables(64)
                .build(),
            grip,
            budget,
            session_id,
            principal: None,
            policy: self.cfg.auth.local_policy.clone(),
            streams: HashMap::new(),
            next_stream_id: 1,
            pending_swaps: Vec::new(),
        };

        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(TICKS_PER_CHECK);
        store.epoch_deadline_callback(|mut ctx| {
            let budget = &mut ctx.data_mut().budget;
            budget.checkpoint();
            match budget.violation() {
                Some(reason) => Err(wasmtime::Error::msg(reason)),
                // `Yield` rather than `Continue`: every entry point here is
                // `*_async`, and a computing guest otherwise holds one of the
                // runtime's few threads solid for its whole budget.
                None => Ok(UpdateDeadline::Yield(TICKS_PER_CHECK)),
            }
        });
        store
    }
}

impl Runtime {
    /// The WASI capabilities a guest is handed, per configuration.
    ///
    /// Anything not granted here is not merely restricted, it is absent: WASI
    /// preview 2 gives a guest nothing by default, so an ungranted capability
    /// shows up as a runtime error rather than a link failure.
    fn wasi_ctx(&self) -> WasiCtx {
        let mut builder = WasiCtxBuilder::new();
        let wasi = &self.cfg.wasi;

        if wasi.network {
            builder.inherit_network();
        }
        builder.allow_ip_name_lookup(wasi.dns);
        if wasi.env {
            builder.inherit_env();
        }
        if wasi.stdio {
            builder.inherit_stdio();
        }

        for dir in &wasi.dirs {
            // A preopen has to exist before it can be handed over, and a
            // missing one would otherwise fail every single call.
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping wasi preopen");
                continue;
            }
            let guest_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "workspace".to_string());

            if let Err(e) =
                builder.preopened_dir(dir, &guest_name, DirPerms::all(), FilePerms::all())
            {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping wasi preopen");
            }
        }

        builder.build()
    }
}

/// A plain fn rather than a closure: the linker needs a higher-ranked
/// signature that closure inference will not produce on its own.
fn host_state(state: &mut HostState) -> &mut HostState {
    state
}

fn build_linker(engine: &Engine, caps: Caps) -> Result<Linker<HostState>> {
    let mut linker: Linker<HostState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(anyhow::Error::from)
        .context("linking wasi")?;

    // `wasi:http` is what makes a web-facing tool possible at all. TLS is
    // terminated here rather than in the guest, because no TLS crate builds for
    // wasm32-wasip2: ring and openssl both need a C toolchain targeting wasm.
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
        .map_err(anyhow::Error::from)
        .context("linking wasi:http")?;

    bindings::sys::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;

    match caps {
        Caps::Agent => {
            bindings::session::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            // Full skill access, including writes. The agent authors skills.
            bindings::skills::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::llm::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::sandbox::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::tooling::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::devkit::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::branch::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            // Host access: the filesystem, shells, and the process itself.
            // Only the agent gets these; tools stay on the sandbox.
            bindings::hostfs::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::terminal::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::control::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            bindings::configuration::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            // Delegation: only the agent may spawn agents. Registered here
            // before `world agent` imports it, which is what lets the contract
            // change land on a host that already answers. Registering an
            // import no guest asks for is harmless — an unused entry in the
            // linker's table.
            bindings::delegation::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            // Transcript recall. Registered here while the interface is still
            // staged, for the same reason delegation was: an import no guest
            // asks for yet is a harmless entry in the linker's table, and
            // having it answered first is what lets the contract change land
            // without a window where the agent cannot instantiate.
            //
            // Agent-only. The chat surface already receives the event log
            // through its own frames and has no use for a second route to it.
            bindings::transcripts::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
        }
        Caps::Gateway => {
            bindings::session::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
            // The read-only view only. A gateway renders skills; it never
            // writes them, so it is not given the interface that could.
            bindings::skills_view::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
        }
        Caps::Tool => {
            bindings::sandbox::add_to_linker::<_, HasSelf<_>>(&mut linker, host_state)?;
        }
    }
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_allows_work_within_limits() {
        let b = Budget::new("turn", Duration::from_secs(10));
        assert!(b.violation().is_none());
    }

    #[test]
    fn a_long_call_is_fine_as_long_as_it_keeps_talking_to_the_host() {
        // What used to trip the wall clock: a turn that runs far longer than
        // any fixed ceiling, yielding at each host call the way a streaming
        // completion or a tool dispatch does.
        let mut b = Budget::new("turn", Duration::from_millis(20));
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(10));
            b.yielded();
            assert!(b.violation().is_none());
        }
        // Well past what a 50ms wall-clock budget would have allowed.
        assert!(b.started.elapsed() > Duration::from_millis(50));
    }

    #[test]
    fn budget_trips_on_spin_without_yielding() {
        let b = Budget::new("turn", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let v = b.violation().expect("should trip");
        assert!(v.contains("infinite loop"), "{v}");
    }

    #[test]
    fn yielding_resets_the_spin_timer() {
        let mut b = Budget::new("turn", Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(15));
        b.yielded();
        std::thread::sleep(Duration::from_millis(15));
        // 30ms elapsed in total but only 15ms since the last host call.
        assert!(b.violation().is_none());
    }

    #[test]
    fn host_time_is_not_charged_to_the_guest() {
        // A slow host import used to be billed as guest spin time, trapping a
        // healthy turn as "likely an infinite loop".
        let mut b = Budget::new("turn", Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(40));
        // The guest was in the host for that whole time.
        b.entered_host("events");
        b.checkpoint();
        assert!(
            b.violation().is_none(),
            "time inside a host import must not count as spinning"
        );
        // But once it stops calling the host, the clock is real again.
        std::thread::sleep(Duration::from_millis(40));
        b.checkpoint();
        assert!(
            b.violation().unwrap().contains("infinite loop"),
            "a guest that stops talking to the host must still trip"
        );
    }

    #[test]
    fn a_stop_is_not_an_instant_violation() {
        // A cooperative guest notices the stop at its next checkpoint and
        // returns cleanly. Trapping it immediately would turn every successful
        // stop into a crash, which is what the watchdog rolls components back
        // for — so the grace window is load-bearing, not politeness.
        let mut b = Budget::new("turn", Duration::from_secs(60));
        b.cancel();
        assert!(b.cancel_requested());
        assert!(
            b.violation().is_none(),
            "a guest must be given the chance to return"
        );
    }

    #[test]
    fn a_guest_that_ignores_a_stop_is_eventually_trapped() {
        let mut b = Budget::new("turn", Duration::from_secs(60));
        b.cancel();
        // Reach back past the grace window rather than sleeping through it.
        b.cancel_requested = Some(Instant::now() - (CANCEL_GRACE + Duration::from_millis(50)));
        let v = b.violation().expect("the window has run out");
        assert!(v.contains("stopped by the user"), "{v}");
    }

    #[test]
    fn repeated_stops_do_not_extend_the_grace_window() {
        // Someone jabbing the stop button must not keep resetting the deadline
        // and thereby let a wedged guest run forever.
        let mut b = Budget::new("turn", Duration::from_secs(60));
        b.cancel_requested = Some(Instant::now() - (CANCEL_GRACE + Duration::from_millis(50)));
        b.cancel();
        assert!(
            b.violation().is_some(),
            "a second stop must not push the deadline out"
        );
    }

    #[test]
    fn a_stopped_guest_is_not_also_accused_of_spinning() {
        // While winding down, the guest is not making host calls. That must
        // read as "stopping", not as an infinite loop — the wrong message here
        // sent every stop to the watchdog as agent misbehaviour.
        let mut b = Budget::new("turn", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            b.violation().unwrap().contains("infinite loop"),
            "spin is still detected when no stop is pending"
        );
        b.cancel();
        assert!(
            b.violation().is_none(),
            "a stopping guest must not be reported as a runaway"
        );
    }

    #[test]
    fn spin_detection_survives_the_grace_window() {
        let mut b = Budget::new("turn", Duration::from_millis(1));
        b.cancel();
        b.cancel_requested = Some(Instant::now() - (CANCEL_GRACE + Duration::from_millis(50)));
        std::thread::sleep(Duration::from_millis(5));
        // Both conditions hold; the stop is the more useful explanation.
        let v = b.violation().expect("should trip");
        assert!(v.contains("stopped by the user"), "{v}");
    }
}
