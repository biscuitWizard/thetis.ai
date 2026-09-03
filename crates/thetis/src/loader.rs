//! The component registry.
//!
//! Holds the compiled component currently active in each aspect. Swapping is a
//! pointer replacement: calls already in flight keep the `Arc` they started
//! with and finish on the old code, while the next call picks up the new one.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::aspect::Aspect;

pub struct LoadedComponent {
    pub aspect: Aspect,
    pub revision: u64,
    /// sha256 of the artifact bytes, for "did anything actually change"
    /// comparisons without a registry.
    pub artifact_sha256: String,
    pub component: Component,
}

#[derive(Default)]
pub struct Loader {
    aspects: RwLock<HashMap<Aspect, Arc<LoadedComponent>>>,
}

impl Loader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compiles a `.wasm` file. Returns an error if it is not a valid component
    /// for this engine — the first gate a candidate build must pass.
    pub fn compile(
        engine: &Engine,
        aspect: &Aspect,
        revision: u64,
        path: &Path,
    ) -> Result<Arc<LoadedComponent>> {
        // Cranelift compiles the whole component here — seconds of pure CPU
        // for a large guest, on whichever runtime thread happened to call.
        // Every call site is inside an `async fn`, so it is offloaded once,
        // here, rather than at each of the seven of them.
        let component = crate::offload::blocking(|| Component::from_file(engine, path))
            .map_err(anyhow::Error::from)
            .with_context(|| format!("compiling {} from {}", aspect, path.display()))?;
        let artifact_sha256 = crate::buildcache::hash_file(path).unwrap_or_default();
        Ok(Arc::new(LoadedComponent {
            aspect: aspect.clone(),
            revision,
            artifact_sha256,
            component,
        }))
    }

    pub fn get(&self, aspect: &Aspect) -> Option<Arc<LoadedComponent>> {
        self.aspects.read().ok()?.get(aspect).cloned()
    }

    pub fn install(&self, component: Arc<LoadedComponent>) {
        if let Ok(mut aspects) = self.aspects.write() {
            aspects.insert(component.aspect.clone(), component);
        }
    }

    pub fn remove(&self, aspect: &Aspect) {
        if let Ok(mut aspects) = self.aspects.write() {
            aspects.remove(aspect);
        }
    }

    /// Every active aspect and its revision, for `/admin` and the agent's own
    /// `history` view.
    pub fn active(&self) -> Vec<(Aspect, u64)> {
        let Ok(aspects) = self.aspects.read() else {
            return Vec::new();
        };
        let mut out: Vec<(Aspect, u64)> = aspects
            .values()
            .map(|c| (c.aspect.clone(), c.revision))
            .collect();
        out.sort_by_key(|(s, _)| s.key());
        out
    }

    pub fn tools(&self) -> Vec<Aspect> {
        let Ok(aspects) = self.aspects.read() else {
            return Vec::new();
        };
        let mut out: Vec<Aspect> = aspects
            .keys()
            .filter(|s| matches!(s, Aspect::Tool(_)))
            .cloned()
            .collect();
        out.sort_by_key(|s| s.key());
        out
    }
}
