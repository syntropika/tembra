//! Share compiled machine code without sharing Orb heaps or capabilities.
use crate::providers::Providers;
use crate::{adapter, failure};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicUsize, Ordering},
};
use tembra::{Orb, Result};
use wasmtime::component::Component;

/// Console output is attributed by the host, never by a guest-provided Orb name.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LogRecord {
    pub orb: String,
    pub level: String,
    pub message: String,
}

pub(crate) type LogSink = Arc<dyn Fn(&LogRecord) + Send + Sync>;

/// Route every Wasmtime compilation through bounded workers without starting the host's global pool.
fn compile<T: Send>(work: impl FnOnce() -> wasmtime::Result<T> + Send) -> Result<T> {
    static COMPILER: LazyLock<std::result::Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>> =
        LazyLock::new(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(
                    std::thread::available_parallelism().map_or(1, |count| count.get().min(4)),
                )
                .thread_name(|index| format!("tembra-compile-{index}"))
                .build()
        });
    COMPILER
        .as_ref()
        .map_err(failure)?
        .install(work)
        .map_err(failure)
}

/// Bounds apply to each component Store and each exported callback.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub memory_bytes: usize,
    pub fuel: u64,
    pub packet_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for Limits {
    /// Supply bounded defaults suitable for a small interpreted Orb.
    fn default() -> Self {
        Self {
            memory_bytes: 128 << 20,
            fuel: 100_000_000,
            packet_bytes: 8 << 20,
            timeout_ms: 10_000,
        }
    }
}

/// Shared compiled code; each prepared Orb receives a separate Store.
#[derive(Clone)]
pub struct Engine {
    pub(crate) engine: wasmtime::Engine,
    compiled: Arc<Mutex<BTreeMap<[u8; 32], Weak<Component>>>>,
    pub(crate) limits: Limits,
    pub(crate) providers: Providers,
    pub(crate) active_instances: Arc<AtomicUsize>,
    /// Exported WIT resources held in compiled Stores on behalf of native owners.
    pub(crate) live_resources: Arc<AtomicUsize>,
    pub(crate) logger: Option<LogSink>,
    /// The private socket stream adapter, compiled on first use.
    pub(crate) stream_adapter: Arc<crate::worker::StreamAdapter>,
}

impl Engine {
    /// Configure Wasmtime limits with native code generation (Pulley on iOS).
    pub fn new(limits: Limits) -> Result<Self> {
        Self::configured(limits, cfg!(target_os = "ios"))
    }

    /// Execute portable bytecode without allocating executable native memory.
    pub fn interpreted(limits: Limits) -> Result<Self> {
        Self::configured(limits, true)
    }

    /// Share the same limits and capabilities across native and interpreted execution.
    fn configured(limits: Limits, interpreted: bool) -> Result<Self> {
        if limits.memory_bytes < 2 << 20
            || limits.memory_bytes > u32::MAX as usize
            || limits.fuel == 0
            || limits.packet_bytes == 0
            || limits.timeout_ms == 0
        {
            return Err(failure(
                "engine limits must be positive and the Wasm memory bound must fit wasm32 with at least 2 MiB",
            ));
        }
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true).consume_fuel(true);
        if interpreted {
            config
                .target(if cfg!(target_pointer_width = "64") {
                    "pulley64"
                } else {
                    "pulley32"
                })
                .map_err(failure)?;
        }
        let engine = wasmtime::Engine::new(&config).map_err(failure)?;
        Ok(Self {
            engine,
            compiled: Arc::default(),
            limits,
            providers: Providers::default(),
            active_instances: Arc::default(),
            live_resources: Arc::default(),
            logger: None,
            stream_adapter: Arc::default(),
        })
    }

    /// Count live Wasm Stores retained by this engine's prepared or installed Orbs.
    pub fn instances(&self) -> usize {
        self.active_instances.load(Ordering::Acquire)
    }

    /// Count exported WIT resources that compiled Stores still hold in their tables for an
    /// owner. An instance stops counting when it leaves its table, which happens before its
    /// destructor runs, or when its Store is torn down. This is a leak diagnostic, not evidence
    /// that destructors succeeded: a failed destructor quarantines its revision and is reported
    /// through that cause and the engine's log.
    pub fn resources(&self) -> usize {
        self.live_resources.load(Ordering::Acquire)
    }

    /// Receive attributed guest diagnostics without allowing observer panics to interrupt execution.
    /// Wall clock for `wasi:clocks/system-clock` and granted source built-ins; one fresh
    /// instance per Store.
    pub fn wall_clock<C: crate::HostWallClock + 'static>(
        mut self,
        factory: impl Fn() -> C + Send + Sync + 'static,
    ) -> Self {
        self.providers.system.wall = Some(Arc::new(move || Box::new(factory())));
        self
    }

    /// Monotonic clock for `wasi:clocks/monotonic-clock` and granted source built-ins.
    pub fn monotonic_clock<C: crate::HostMonotonicClock + 'static>(
        mut self,
        factory: impl Fn() -> C + Send + Sync + 'static,
    ) -> Self {
        self.providers.system.monotonic = Some(Arc::new(move || Box::new(factory())));
        self
    }

    /// Cryptographically secure source for `wasi:random/random`; must be a CSPRNG.
    pub fn secure_random<R: crate::Rng + Send + 'static>(
        mut self,
        factory: impl Fn() -> R + Send + Sync + 'static,
    ) -> Self {
        self.providers.system.secure = Some(Arc::new(move |context| {
            context.secure_random(factory());
        }));
        self
    }

    /// Insecure source for `wasi:random/insecure`.
    pub fn insecure_random<R: crate::Rng + Send + 'static>(
        mut self,
        factory: impl Fn() -> R + Send + Sync + 'static,
    ) -> Self {
        self.providers.system.insecure = Some(Arc::new(move |context| {
            context.insecure_random(factory());
        }));
        self
    }

    /// Seed for `wasi:random/insecure-seed` and the random-granted source built-in PRNG.
    pub fn insecure_random_seed(
        mut self,
        factory: impl Fn() -> u128 + Send + Sync + 'static,
    ) -> Self {
        self.providers.system.seed = Some(Arc::new(factory));
        self
    }

    pub fn on_log(mut self, sink: impl Fn(&LogRecord) + Send + Sync + 'static) -> Self {
        self.logger = Some(Arc::new(sink));
        self
    }

    /// Register a directory provider without exposing it to any Orb until installation grants it.
    pub fn filesystem(
        mut self,
        name: impl Into<String>,
        directory: impl Into<std::path::PathBuf>,
    ) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty()
            || name == tembra_contracts::HTTP_HANDLER
            || self.providers.filesystem.contains_key(&name)
        {
            return Err(failure(
                "filesystem provider names must be nonempty, unique and not reserved",
            ));
        }
        self.providers.filesystem.insert(name, directory.into());
        Ok(self)
    }

    /// Register the host's TCP transport without granting any Orb a destination.
    pub fn network(mut self, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty()
            || name == tembra_contracts::HTTP_HANDLER
            || !self.providers.network.insert(name)
        {
            return Err(failure(
                "network provider names must be nonempty, unique and not reserved",
            ));
        }
        Ok(self)
    }

    /// Reuse live compiled code while fresh Stores retain all mutable state and permissions.
    pub(crate) fn compiled(&self, bytes: &[u8]) -> Result<Arc<Component>> {
        let key: [u8; 32] = Sha256::digest(bytes).into();
        let mut cache = self.compiled.lock().map_err(failure)?;
        if let Some(component) = cache.get(&key).and_then(Weak::upgrade) {
            return Ok(component);
        }
        cache.retain(|_, component| component.strong_count() != 0);
        let component = Arc::new(compile(|| Component::new(&self.engine, bytes))?);
        cache.insert(key, Arc::downgrade(&component));
        Ok(component)
    }

    /// Materialize one closed artifact through the common native Orb installation model.
    pub fn load(&self, bytes: &[u8], install: crate::Install) -> Result<Orb> {
        let artifact = tembra_contracts::Artifact::decode(bytes).map_err(failure)?;
        let bindings = artifact
            .contract
            .providers(&install.requires, &install.grants)?;
        let port = self.open(
            &artifact,
            &bindings,
            &install.grants,
            &install.cli,
            crate::port::Transport::Capabilities,
        )?;
        adapter::orb(
            port.metadata,
            port.session,
            port.services,
            artifact.contract.clone(),
            &bindings,
            port.hooks,
            port.ingress,
            port.capabilities,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_engines_reuse_live_code_without_mutating_each_others_configuration() {
        let engine = Engine::new(Limits::default()).unwrap();
        let bytes = b"\0asm\x0d\0\x01\0";
        let first = engine.compiled(bytes).unwrap();
        let clone = engine.clone();
        let second = clone.compiled(bytes).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(engine.instances(), 0);
    }
}
