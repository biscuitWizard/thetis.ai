//! The gateway role: the stable front of the system.
//!
//! Owns the listener, the database, and the worker fleet. Deliberately hosts
//! as little as possible that a conversation can rewrite: the one guest it
//! runs is the UI gateway component, and even that it only ever loads from
//! artifacts a worker has already built and smoke-tested.

use anyhow::Result;
use std::sync::Arc;

use crate::aspect::Aspect;
use crate::config::Config;
use crate::grip::Grip;
use crate::loader::Loader;
use crate::runtime::Runtime;
use crate::store::Store;
use crate::web;
use crate::workers::{self, WorkerRouter};

pub async fn run() -> Result<()> {
    let cfg = Arc::new(Config::load()?);
    tracing::info!(root = %cfg.root.display(), "starting thetis (gateway)");
    // The zero point for the uptime the status toolbar shows.
    crate::system_api::mark_start();
    // Before anything in this process can rebuild the kernel: from here on the
    // gateway spawns workers from a copy of its own binary that a later build
    // cannot pull out from under it.
    crate::control::pin_self_exe(&cfg);

    let runtime = Runtime::new(cfg.clone())?;
    let db = Arc::new(Store::open(&cfg.db_path())?);
    let router = WorkerRouter::new();
    let grip = Grip::gateway(cfg.clone(), runtime, db.clone(), router.clone())?;

    // Trunk is where the kernel's own WIT came from, so these agreeing is the
    // normal case — but `rebuild-kernel.sh` deliberately keeps the previous
    // binary when a build fails, and that is exactly how a running kernel ends
    // up older than the contract in the checkout. Said once, up front: it
    // explains every guest that will not load afterwards, here or in a worker.
    //
    // Diagnosis only, and structurally so: a gateway grip carries no `GitCtl`,
    // so the reconcile stops at `Unrepairable` rather than merging. That is the
    // right stopping point — the checkout it reads *is* trunk, so a mismatch
    // here is a stale binary, and no branch this process could move would fix
    // it.
    crate::pipeline::reconcile_wit_contract(&grip)
        .await
        .report();

    // Serve the UI from the last activated build straight away; if there is
    // none yet, the fallback page covers the gap until the worker's first
    // build lands and announces itself.
    load_ui_gateway(&grip).await;
    bootstrap_ui_if_missing(grip.clone());

    // Workers spawn lazily, one per conversation, when a message arrives.
    // The boot sweep prunes checkouts lost to a crash, and interrupted turns
    // are repaired and resumed — resuming is itself what re-materializes the
    // workers they need.
    let branches = crate::branches::Branches::new(cfg.clone(), db.clone());
    if let Err(e) = branches.reconcile_on_boot().await {
        tracing::warn!(error = %e, "worktree sweep failed");
    }
    // Private tools and skills stay tracked locally; the pre-push guard is
    // what keeps them off remotes. Installed every boot, content-checked.
    if let Err(e) = crate::publish::install_push_guard(branches.root_git()).await {
        tracing::warn!(error = %e, "could not install the publish guard");
    }
    crate::offload::spawn_stall_detector();
    workers::spawn_reaper(router.clone());
    {
        let grip = grip.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let now = crate::store::now_ms();
                if let Some(store) = grip.local_store() {
                    match store.prune_expired_logins(now) {
                        Ok(n) if n > 0 => tracing::debug!(count = n, "pruned expired logins"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "could not prune expired logins"),
                    }
                }
            }
        });
    }

    // The headless browser behind the `web-browser-*` tools. Set up and
    // supervised here because the gateway outlives every worker, so one browser
    // serves all conversations and survives their restarts. Non-fatal by
    // design: it checks node, installs the pinned Playwright if the vendored
    // copy is missing, and reports through the tools rather than stopping boot.
    crate::browser::spawn(cfg.clone());
    // Before anything is resumed: a sub-agent recorded as running cannot be,
    // because nothing has started yet. Those rows are the wreckage of whatever
    // restart brought us here, and nothing else will ever clear them — a child
    // session is not a conversation, so the resume scan below does not look at
    // it. Left alone they are something a parent can wait on until the wait cap
    // expires, every time, forever.
    if let Some(store) = grip.local_store() {
        match crate::subagents::Subagents::new(store)
            .fail_orphans("its turn died with an orchestrator restart and was never resumed")
        {
            Ok(swept) if !swept.is_empty() => {
                tracing::info!(
                    count = swept.len(),
                    "settled sub-agents orphaned by a restart"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "could not sweep orphaned sub-agents"),
        }
    }
    {
        let grip = grip.clone();
        tokio::spawn(async move {
            workers::reconcile_and_resume(&grip).await;
        });
    }

    // Ownership, backfilled every boot. In local mode everything belongs to the
    // one implicit administrator; in users mode it belongs to whoever
    // `auth.claim_unowned` names.
    //
    // The `stale` argument is what makes the second of those reachable. Local
    // mode stamps `LOCAL_OWNER` on every conversation, so a system that has
    // ever booted without users has nothing unowned left to claim — and
    // turning users on would hand the new account an empty sidebar and
    // "conversation belongs to another user" for every conversation it used to
    // have, held by an owner that cannot log in. So in users mode the sentinel
    // counts as unclaimed, and the history moves across with the switch.
    if let Some(store) = grip.local_store() {
        let (claim, stale) = if cfg.auth.users_mode {
            (
                cfg.auth.claim_unowned.as_str(),
                Some(crate::auth::LOCAL_OWNER),
            )
        } else {
            (crate::auth::LOCAL_OWNER, None)
        };
        let claiming = store.sessions_needing_an_owner(stale)?;
        for id in &claiming {
            store.set_owner(id, claim)?;
        }
        if !claiming.is_empty() {
            tracing::info!(
                count = claiming.len(),
                owner = claim,
                "claimed legacy conversations"
            );
        }
    }

    if grip.persist.list_sessions(true).await?.is_empty() {
        let owner = if cfg.auth.users_mode {
            cfg.auth.claim_unowned.as_str()
        } else {
            crate::auth::LOCAL_OWNER
        };
        let first = grip
            .persist
            .create_session(Some("Welcome".into()), &cfg.default_mode, owner)
            .await?;
        tracing::info!(session = %first.id, "created first session");
    }

    // The Discord bot, if one is configured. A refusal here is deliberate and
    // not fatal: the connector declines to start when its mode is missing or
    // not read-only, and the rest of Thetis should still come up so the
    // configuration can be fixed.
    if let Err(e) = crate::discord::spawn(grip.clone()) {
        tracing::error!(error = %format!("{e:#}"), "the Discord connector did not start");
    }

    tracing::info!("open http://{} in a browser", cfg.bind_addr);
    web::serve(grip).await
}

/// Loads the UI gateway component for trunk's current head.
///
/// The build cache is keyed by content, so whichever branch built this exact
/// tree already paid for the artifact. Falls back to the legacy revision
/// registry for deployments whose cache has not been populated yet, and to
/// the host-rendered page when neither has anything.
pub async fn load_ui_gateway(grip: &Arc<Grip>) {
    let aspect = Aspect::gateway(&grip.cfg.primary_gateway);

    let trunk = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
    if let Some(key) = crate::pipeline::cache_key_with(&trunk, &grip.cfg, "HEAD", &aspect).await {
        if let Ok(Some(meta)) = grip.buildcache.lookup(&aspect.key(), &key) {
            match grip
                .buildcache
                .artifact_path(&meta, crate::pipeline::CACHE_ARTIFACT)
                .and_then(|artifact| {
                    Loader::compile(
                        &grip.runtime.engine,
                        &aspect,
                        crate::pipeline::key_revision(&key),
                        &artifact,
                    )
                }) {
                Ok(component) => {
                    grip.loader.install(component);
                    tracing::info!(%aspect, "serving trunk's UI from the build cache");
                    return;
                }
                Err(e) => {
                    tracing::warn!(%aspect, error = %e, "cached UI artifact would not load");
                }
            }
        }
    }

    // Legacy fallback: the last activated revision from before the cache.
    // Smoke-tested first — an artifact built against an older WIT contract
    // can compile yet fail at instantiation, and serving it would block the
    // bootstrap build that actually fixes things.
    let active = grip.revisions.active(&aspect).await.ok().flatten();
    let Some(active) = active else {
        tracing::info!(%aspect, "no UI build on record yet; serving the fallback page until one lands");
        return;
    };
    let artifact = grip.revisions.component_path(&aspect, active.revision);
    match Loader::compile(&grip.runtime.engine, &aspect, active.revision, &artifact) {
        Ok(component) => {
            if let Err(e) = crate::pipeline::smoke_test(grip, &aspect, &component.component).await {
                tracing::warn!(%aspect, error = %e,
                    "legacy UI artifact no longer runs against this kernel; a fresh build will replace it");
                return;
            }
            grip.loader.install(component);
            tracing::info!(%aspect, revision = active.revision, "serving the UI (legacy artifact)");
        }
        Err(e) => {
            tracing::warn!(%aspect, error = %e, "stored UI artifact would not load");
        }
    }
}

/// Builds the UI gateway once, in the background, when no artifact exists
/// anywhere. Bootstrap only: every later UI build happens in a conversation's
/// worker and reaches trunk by merging — but on a truly fresh deployment
/// nothing else can break the deadlock, because workers only spawn for
/// messages that arrive through this very UI.
fn bootstrap_ui_if_missing(grip: Arc<Grip>) {
    let aspect = Aspect::gateway(&grip.cfg.primary_gateway);
    if grip.loader.get(&aspect).is_some() {
        return;
    }
    tokio::spawn(async move {
        tracing::info!(%aspect, "no UI build exists anywhere; bootstrapping one from trunk");
        let build = match grip.builder.build(&grip.cfg, &aspect).await {
            Ok(build) if build.success => build,
            Ok(build) => {
                tracing::error!(%aspect, "bootstrap build failed:\n{}", build.stderr);
                return;
            }
            Err(e) => {
                tracing::error!(%aspect, error = %e, "bootstrap build did not run");
                return;
            }
        };
        let Some(wasm) = build.wasm_path else { return };

        let trunk = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
        let key = crate::pipeline::cache_key_with(&trunk, &grip.cfg, "HEAD", &aspect).await;
        let revision = key
            .as_deref()
            .map(crate::pipeline::key_revision)
            .unwrap_or(0);
        let component = match Loader::compile(&grip.runtime.engine, &aspect, revision, &wasm) {
            Ok(component) => component,
            Err(e) => {
                tracing::error!(%aspect, error = %e, "bootstrap build is not a valid component");
                return;
            }
        };
        if let Err(e) = crate::pipeline::smoke_test(&grip, &aspect, &component.component).await {
            tracing::error!(%aspect, error = %e, "bootstrap build failed its smoke test");
            return;
        }
        // Cache it under trunk's tree — only when the tree is clean, or the
        // key would mislabel the artifact.
        if let (Some(key), Ok(false)) = (&key, trunk.is_dirty().await) {
            let rel = grip.cfg.aspect_source_rel(&aspect).unwrap_or_default();
            let meta = crate::buildcache::BuildMeta {
                aspect: aspect.key(),
                key: key.clone(),
                artifact_sha256: component.artifact_sha256.clone(),
                smoke: crate::buildcache::SmokeVerdict::Pass,
                source_commit: trunk.head().await.unwrap_or_default(),
                aspect_tree: trunk
                    .tree_oid("HEAD", &rel)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                wit_tree: trunk
                    .tree_oid("HEAD", "wit")
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                created_ms: crate::buildcache::now_ms(),
                note: "bootstrap".to_string(),
            };
            let _ = grip
                .buildcache
                .store(&wasm, crate::pipeline::CACHE_ARTIFACT, &meta);
        }
        grip.loader.install(component);
        tracing::info!(%aspect, "UI bootstrapped and serving");
    });
}
