//! Calling into the gateway component.
//!
//! The host owns the socket; the gateway owns every byte the user sees. Asset
//! requests and client messages are infrequent, so each gets a fresh store.
//! Event rendering is not — a streaming reply renders one frame per token — so
//! the renderer keeps a warm instance and recycles it on swap, trap, or age.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use wasmtime::Store;

use crate::aspect::Aspect;
use crate::bindings::gateway::{Gateway, GatewayAction};
use crate::bindings::types::{Asset, OutboundEvent};
use crate::grip::Grip;
use crate::runtime::{Budget, Caps, HostState};

/// Rebuild the warm renderer instance after this many calls, so a long-lived
/// guest cannot accumulate unbounded state.
const RENDERER_MAX_CALLS: u64 = 5_000;

/// The gateway serving the browser UI, named in configuration.
fn gateway_aspect(grip: &Arc<Grip>) -> Aspect {
    Aspect::gateway(&grip.cfg().primary_gateway)
}

async fn fresh_instance(
    grip: &Arc<Grip>,
    label: &str,
    principal: Option<Arc<crate::auth::Principal>>,
) -> Result<(Store<HostState>, Gateway, u64)> {
    let loaded = grip
        .loader
        .get(&gateway_aspect(grip))
        .context("no gateway component is loaded")?;
    instance_of(grip, label, loaded, principal).await
}

/// As [`fresh_instance`], against a component chosen by the caller.
///
/// The preview route serves a *conversation's* gateway build rather than the
/// one this process has loaded, so the component cannot come from the loader.
async fn instance_of(
    grip: &Arc<Grip>,
    label: &str,
    loaded: Arc<crate::loader::LoadedComponent>,
    principal: Option<Arc<crate::auth::Principal>>,
) -> Result<(Store<HostState>, Gateway, u64)> {
    let budget = Budget::probe(label, grip.cfg().probe_budget);
    let mut store = if let Some(p) = principal {
        grip.gateway_store(budget, p)
    } else {
        grip.runtime
            .new_store(grip.clone(), Caps::Gateway, budget, None)
    };

    let instance = Gateway::instantiate_async(
        &mut store,
        &loaded.component,
        grip.runtime.linker(Caps::Gateway),
    )
    .await
    .map_err(anyhow::Error::from)
    .context("instantiating gateway")?;

    Ok((store, instance, loaded.revision))
}

pub async fn serve_asset(grip: &Arc<Grip>, path: &str) -> Result<Option<Asset>> {
    let (mut store, gw, _) = fresh_instance(grip, "gateway serve-asset", None).await?;
    gw.call_serve_asset(&mut store, path)
        .await
        .map_err(anyhow::Error::from)
        .context("gateway serve-asset")
}

/// Serves an asset from one conversation's own gateway build.
///
/// The UI a browser loads is trunk's, deliberately — but that left an agent
/// working on the UI unable to see its own work: the build goes green and
/// nothing visibly changes, which is worse than an error. One conversation
/// resorted to launching a second orchestrator on another port and driving it
/// with browser automation, which was the only way to look at its own output.
pub async fn serve_preview_asset(
    grip: &Arc<Grip>,
    loaded: Arc<crate::loader::LoadedComponent>,
    path: &str,
) -> Result<Option<Asset>> {
    let (mut store, gw, _) = instance_of(grip, "gateway preview serve-asset", loaded, None).await?;
    gw.call_serve_asset(&mut store, path)
        .await
        .map_err(anyhow::Error::from)
        .context("gateway preview serve-asset")
}

pub async fn on_client_message(
    grip: &Arc<Grip>,
    client_id: &str,
    frame_json: &str,
    principal: Arc<crate::auth::Principal>,
) -> Result<Vec<GatewayAction>> {
    let (mut store, gw, _) =
        fresh_instance(grip, "gateway on-client-message", Some(principal)).await?;
    gw.call_on_client_message(&mut store, client_id, frame_json)
        .await
        .map_err(anyhow::Error::from)
        .context("gateway on-client-message")
}

/// Warm renderer used by the single fan-out task.
pub struct Renderer {
    grip: Arc<Grip>,
    warm: Option<Warm>,
}

struct Warm {
    revision: u64,
    store: Store<HostState>,
    instance: Gateway,
    calls: u64,
}

impl Renderer {
    pub fn new(grip: Arc<Grip>) -> Self {
        Self { grip, warm: None }
    }

    /// True when the cached instance is stale: a swap happened, it aged out, or
    /// there is nothing cached yet.
    fn needs_refresh(&self) -> bool {
        let Some(warm) = &self.warm else {
            return true;
        };
        if warm.calls >= RENDERER_MAX_CALLS {
            return true;
        }
        match self.grip.loader.get(&gateway_aspect(&self.grip)) {
            Some(current) => current.revision != warm.revision,
            None => true,
        }
    }

    pub async fn render(&mut self, event: OutboundEvent) -> Option<String> {
        if self.needs_refresh() {
            match fresh_instance(&self.grip, "gateway render-event", None).await {
                Ok((store, instance, revision)) => {
                    self.warm = Some(Warm {
                        revision,
                        store,
                        instance,
                        calls: 0,
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cannot render events: gateway unavailable");
                    self.warm = None;
                    return None;
                }
            }
        }

        let warm = self.warm.as_mut()?;
        warm.calls += 1;
        // The store outlives a single call, so its budget must be rearmed: the
        // last-yield mark keeps ageing between events, and would eventually
        // read as a guest that had stopped talking to the host.
        warm.store.data_mut().budget =
            Budget::probe("gateway render-event", self.grip.cfg().probe_budget);

        match warm
            .instance
            .call_render_event(&mut warm.store, &event)
            .await
        {
            Ok(frame) => frame,
            Err(trap) => {
                // A trapped store is poisoned; drop it so the next event starts clean.
                tracing::warn!(error = %trap, "gateway render-event trapped");
                self.warm = None;
                None
            }
        }
    }
}

// --- preview -----------------------------------------------------------------

/// Compiled preview gateways, keyed by the cache key they were built from.
///
/// Compiling a component costs seconds of cranelift, and a browser loading a
/// page asks for a dozen assets. Keyed by cache key rather than by session so
/// a branch that has not moved reuses its compile, and one that has moved
/// naturally misses.
static PREVIEWS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, Arc<crate::loader::LoadedComponent>>>,
> = std::sync::OnceLock::new();

/// The gateway build belonging to one conversation's branch.
///
/// Read from the shared, content-addressed build cache, so it works whether or
/// not that conversation currently has a worker: the artifact was filed when
/// the branch built it and is keyed by the source tree, not by who built it.
pub async fn preview_component(
    grip: &Arc<Grip>,
    session_id: &str,
) -> Result<Arc<crate::loader::LoadedComponent>> {
    let store = grip
        .local_store()
        .context("previews are a gateway concern")?;
    let branches = crate::branches::Branches::new(grip.cfg().clone(), store.clone());
    let row = branches
        .get(session_id)?
        .with_context(|| format!("{session_id} has no branch yet, so it has nothing to preview"))?;

    let aspect = gateway_aspect(grip);
    let key =
        crate::pipeline::cache_key_with(branches.root_git(), &grip.cfg(), &row.branch_ref, &aspect)
            .await
            .context("could not key this branch's gateway build")?;

    if let Some(hit) = PREVIEWS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|m| m.get(&key).cloned())
    {
        return Ok(hit);
    }

    let meta = grip
        .buildcache
        .lookup(&aspect.key(), &key)?
        .with_context(|| {
            format!(
                "this conversation has not built {aspect} yet, so there is nothing to preview. \
                 Edit it and let the dev kit rebuild it, then reload."
            )
        })?;
    let artifact = grip
        .buildcache
        .artifact_path(&meta, crate::pipeline::CACHE_ARTIFACT)?;
    let component = crate::loader::Loader::compile(
        &grip.runtime.engine,
        &aspect,
        crate::pipeline::key_revision(&key),
        &artifact,
    )?;

    if let Ok(mut cache) = PREVIEWS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
    {
        // Bounded: one entry per distinct source tree, and a long-lived
        // gateway would otherwise hold every revision an agent ever built.
        if cache.len() > 8 {
            cache.clear();
        }
        cache.insert(key, component.clone());
    }
    Ok(component)
}
