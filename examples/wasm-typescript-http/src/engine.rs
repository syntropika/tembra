//! Consumer bindings reuse the experimental generic Orb adapter and invocation leases.
use crate::Job;
use crate::adapter::{
    PreparedGuard,
    imports::{self, Imports, InvocationToken, Lease, Registry},
};
use futures::{FutureExt, executor::LocalSpawner, lock::Mutex, task::LocalSpawnExt};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::{Rc, Weak},
};
use tembra::{AcquisitionOptions, Code, Context, Error, Result, core::Requirement};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker, Resource},
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "assets/engine.wit",
        world: "engine",
        imports: { default: async | trappable },
        exports: { default: async },
        with: { "tembra:runtime/context.invocation": InvocationToken },
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
    pub use crate::adapter::imports::InvocationToken;
}
pub use bindings::example::readings::capabilities::{Progress, Response};
pub use bindings::exports::example::readings::analysis::{Report, Request};
use bindings::{example::readings::capabilities, tembra::runtime::context as invocation};
pub const CAPABILITIES: &str = "example.capabilities";

enum Operation {
    Check,
    Get(String),
    Publish(Progress),
}
enum Reply {
    Checked,
    Response(Response),
    Published,
}
type StoreData = Imports<Operation, Reply>;

fn failure(error: impl std::fmt::Display) -> Error {
    Error::new(Code::Unavailable, "example.component", format!("{error:#}"))
}
fn declared(error: Error) -> capabilities::Failure {
    match error.code {
        Code::Forbidden => capabilities::Failure::Forbidden,
        Code::Cancelled => capabilities::Failure::Cancelled,
        _ => capabilities::Failure::Unavailable,
    }
}
impl invocation::Host for StoreData {
    async fn check(&mut self, context: Resource<InvocationToken>) -> wasmtime::Result<bool> {
        Ok(matches!(
            self.request(context, Operation::Check).await,
            Ok(Reply::Checked)
        ))
    }
}
impl invocation::HostInvocation for StoreData {
    async fn drop(&mut self, context: Resource<InvocationToken>) -> wasmtime::Result<()> {
        self.revoke(context)
    }
}
impl capabilities::Host for StoreData {
    async fn get(
        &mut self,
        context: Resource<InvocationToken>,
        path: String,
    ) -> wasmtime::Result<std::result::Result<Response, capabilities::Failure>> {
        Ok(match self.request(context, Operation::Get(path)).await {
            Ok(Reply::Response(response)) => Ok(response),
            Ok(_) => Err(capabilities::Failure::Unavailable),
            Err(error) => Err(declared(error)),
        })
    }
    async fn publish(
        &mut self,
        context: Resource<InvocationToken>,
        event: Progress,
    ) -> wasmtime::Result<std::result::Result<(), capabilities::Failure>> {
        Ok(
            match self.request(context, Operation::Publish(event)).await {
                Ok(Reply::Published) => Ok(()),
                Ok(_) => Err(capabilities::Failure::Unavailable),
                Err(error) => Err(declared(error)),
            },
        )
    }
}

pub struct EngineHost {
    engine: Engine,
    component: Component,
}
struct Session {
    store: Mutex<Store<StoreData>>,
    guest: bindings::Engine,
    registry: Rc<Registry>,
    quarantined: Cell<bool>,
}
pub struct Inspection {
    session: Weak<Session>,
    pub registry: Rc<Registry>,
    pub actor_stopped: Rc<Cell<bool>>,
}
impl Inspection {
    pub fn released(&self) -> bool {
        self.session.upgrade().is_none()
    }
    pub async fn resources_empty(&self) -> bool {
        match self.session.upgrade() {
            Some(session) => session.store.lock().await.data().is_empty(),
            None => true,
        }
    }
}
impl EngineHost {
    pub fn new(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .concurrency_support(false)
            .consume_fuel(true);
        let engine = Engine::new(&config)?;
        let component = Component::new(&engine, bytes)?;
        Ok(Self { engine, component })
    }
    pub async fn prepare(
        &self,
        spawner: &LocalSpawner,
        name: &str,
        source: &str,
    ) -> anyhow::Result<(PreparedGuard<Job, Job>, Inspection)> {
        let (data, receive) = Imports::channel(2);
        let mut linker = Linker::<StoreData>::new(&self.engine);
        bindings::Engine::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |state| state,
        )?;
        let mut store = Store::new(&self.engine, data);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(100_000_000_000)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        let guest =
            bindings::Engine::instantiate_async(&mut store, &self.component, &linker).await?;
        store.set_fuel(1_000_000_000)?;
        guest
            .jco_script_engine_script_loader()
            .call_load(&mut store, name, source)
            .await?
            .map_err(anyhow::Error::msg)?;
        let registry = Registry::new();
        let session = Rc::new(Session {
            store: Mutex::new(store),
            guest,
            registry: registry.clone(),
            quarantined: Cell::new(false),
        });
        let actor_stopped = Rc::new(Cell::new(false));
        let stopped = actor_stopped.clone();
        let actor_registry = registry.clone();
        spawner.spawn_local(async move {
            imports::serve(receive, actor_registry, |context, operation| {
                async move {
                    match operation {
                        Operation::Check => Ok(Reply::Checked),
                        Operation::Get(path) => {
                            let service = context.get(
                                CAPABILITIES,
                                vec!["main".into()],
                                AcquisitionOptions::default(),
                            )?;
                            Ok(Reply::Response(service.call(&context, "get", path).await?))
                        }
                        Operation::Publish(event) => {
                            let service = context.get(
                                CAPABILITIES,
                                vec!["main".into()],
                                AcquisitionOptions::default(),
                            )?;
                            service.call::<_, ()>(&context, "publish", event).await?;
                            Ok(Reply::Published)
                        }
                    }
                }
                .boxed_local()
            })
            .await;
            stopped.set(true);
        })?;
        let run = session.clone();
        let prepared = PreparedGuard::from_async(move |context, input| {
            let session = run.clone();
            async move { session.run(context, input).await }.boxed_local()
        })
        .requires(Requirement {
            orb: CAPABILITIES.into(),
            api_version: 1,
            methods: BTreeMap::from([(
                CAPABILITIES.into(),
                BTreeSet::from(["get".into(), "publish".into()]),
            )]),
            hooks: BTreeSet::new(),
        });
        Ok((
            prepared,
            Inspection {
                session: Rc::downgrade(&session),
                registry,
                actor_stopped,
            },
        ))
    }
}
impl Session {
    async fn run(&self, context: Context, mut input: Job) -> Result<Job> {
        self.registry.check_entry(&context)?;
        let mut store = self.store.lock().await;
        context.check()?;
        if self.quarantined.get() {
            return Err(failure("component needs replacement after a trap"));
        }
        store.set_fuel(100_000_000).map_err(failure)?;
        let lease = self.registry.enter(context.clone())?;
        let handle = store.data_mut().issue(&lease).map_err(failure)?;
        let borrowed = Resource::new_borrow(handle.rep());
        let mut active = ActiveCall {
            store: &mut store,
            handle: Some(handle),
            lease: Some(lease),
            quarantined: &self.quarantined,
            settled: false,
        };
        let output = self
            .guest
            .example_readings_analysis()
            .call_run(&mut *active.store, borrowed, &input.request)
            .await;
        active.finish().map_err(failure)?;
        context.check()?;
        match output {
            Ok(Ok(report)) => {
                input.report = Some(report);
                Ok(input)
            }
            Ok(Err(message)) => Err(Error::invalid("example.analysis", message)),
            Err(error) => {
                self.quarantined.set(true);
                Err(failure(error))
            }
        }
    }
}
struct ActiveCall<'a> {
    store: &'a mut Store<StoreData>,
    handle: Option<Resource<InvocationToken>>,
    lease: Option<Lease>,
    quarantined: &'a Cell<bool>,
    settled: bool,
}
impl ActiveCall<'_> {
    fn finish(&mut self) -> wasmtime::Result<()> {
        let handle = self
            .handle
            .take()
            .ok_or_else(|| wasmtime::format_err!("missing invocation handle"))?;
        let cleanup = self.store.data_mut().revoke(handle);
        self.lease.take();
        self.settled = cleanup.is_ok();
        cleanup
    }
}
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.quarantined.set(true);
            if let Some(handle) = self.handle.take() {
                let _ = self.store.data_mut().revoke(handle);
            }
            self.lease.take();
        }
    }
}
