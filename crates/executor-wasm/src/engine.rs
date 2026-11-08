//! Share compiled machine code without sharing Orb heaps or capabilities.
use crate::providers::{Grants, Providers};
use crate::{adapter, failure, worker};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tembra::{Orb, Result};
use tembra_contracts::{Binding, Contract};
use wasmtime::component::Component;

/// Console output is attributed by the host, never by a guest-provided Orb name.
#[derive(Clone, Debug)]
pub struct LogRecord {
    pub orb: String,
    pub level: String,
    pub message: String,
}

pub(crate) type LogSink = Arc<dyn Fn(&LogRecord) + Send + Sync>;

/// Bounds apply to each interpreter instance and each exported callback.
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

/// A reusable compiled interpreter; each prepared Orb receives a separate Store and VM.
#[derive(Clone)]
pub struct Engine {
    pub(crate) engine: wasmtime::Engine,
    pub(crate) component: Arc<Component>,
    pub(crate) limits: Limits,
    pub(crate) providers: Providers,
    pub(crate) active_instances: Arc<AtomicUsize>,
    pub(crate) logger: Option<LogSink>,
}

impl Engine {
    /// Compile an explicitly supplied reusable interpreter component once.
    pub fn new(component: &[u8], limits: Limits) -> Result<Self> {
        if limits.memory_bytes < 2 << 20
            || limits.memory_bytes > u32::MAX as usize
            || limits.fuel == 0
            || limits.packet_bytes == 0
            || limits.timeout_ms == 0
        {
            return Err(failure(
                "interpreter limits must be positive and the Wasm memory bound must fit wasm32 with at least 2 MiB",
            ));
        }
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true).consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).map_err(failure)?;
        let component = Arc::new(Component::new(&engine, component).map_err(failure)?);
        Ok(Self {
            engine,
            component,
            limits,
            providers: Providers::default(),
            active_instances: Arc::default(),
            logger: None,
        })
    }

    /// Count live Wasm Stores retained by this engine's prepared or installed Orbs.
    pub fn instances(&self) -> usize {
        self.active_instances.load(Ordering::Acquire)
    }

    /// Receive attributed guest diagnostics without allowing observer panics to interrupt execution.
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
        if name.trim().is_empty() || self.providers.filesystem.contains_key(&name) {
            return Err(failure(
                "filesystem provider names must be nonempty and unique",
            ));
        }
        self.providers.filesystem.insert(name, directory.into());
        Ok(self)
    }

    /// Register the host's TCP transport without granting any Orb a destination.
    pub fn network(mut self, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() || !self.providers.network.insert(name) {
            return Err(failure(
                "network provider names must be nonempty and unique",
            ));
        }
        Ok(self)
    }

    /// Prepare an isolated TypeScript revision against its WIT contract and provider bindings.
    pub fn prepare(
        &self,
        contract: &Contract,
        name: &str,
        api_version: u32,
        source: &str,
        bindings: &BTreeMap<String, Binding>,
    ) -> Result<Orb> {
        self.prepare_with_grants(
            contract,
            name,
            api_version,
            source,
            bindings,
            &Grants::default(),
        )
    }

    /// Resolve standard WASI providers using this installation's separately supplied Orb grants.
    pub fn prepare_with_grants(
        &self,
        contract: &Contract,
        name: &str,
        api_version: u32,
        source: &str,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<Orb> {
        let plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        contract.check_value_profile().map_err(failure)?;
        for (binding, provider) in bindings {
            if matches!(provider, Binding::Orb { .. }) {
                contract
                    .check_import_value_profile(binding)
                    .map_err(failure)?;
            }
        }
        let context = self.providers.context(&plan.host_imports, grants)?;
        let session = worker::Session::start(self.clone(), context, name.to_owned())?;
        let description = session.load(source, contract.describe())?;
        adapter::orb(plan.orb, session, description, contract.clone(), bindings)
    }
}
