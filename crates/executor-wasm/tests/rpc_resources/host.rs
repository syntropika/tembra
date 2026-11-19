//! Actual native and compiled providers behind the public Rust RPC byte server.
use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    executor::{LocalPool, block_on},
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    io::{self, BufRead, Write},
    path::Path,
    rc::Rc,
};
use tembra::{Host, Orb, Provider, Result, Service, rpc};
use tembra_contracts::{Contract, EmbeddedManifest, ModuleContract};
use tembra_executor_wasm::{Engine, Install, Limits};
use tembra_orb_native_fixture::rpc_provider::provider;

const CODEC: &str = "example:rpc-resources/codec@1.0.0";
const GATE: &str = "example:rpc-resources/gate@1.0.0";

mod shapes;
mod transport;

/// Select either executor behind the same portable RPC server and WIT contract.
fn main() -> Result<()> {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let mode = std::env::args().nth(1).unwrap_or_else(|| "native".into());
    let create: Rc<dyn Fn() -> Result<Orb>> = if mode == "wasm" {
        let engine = Engine::new(Limits::default())?;
        let bytes =
            std::fs::read(std::env::var_os("TEMBRA_RPC_COMPONENT").expect("RPC component path"))
                .expect("compiled RPC provider");
        Rc::new(move || {
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/runtime-fixture");
            let consumed = EmbeddedManifest::read(&bytes)
                .expect("embedded WIT")
                .wit
                .map(|path| base.join(path));
            let module = ModuleContract::from_component(&bytes, consumed.as_deref())
                .expect("component contract")
                .expect("macro module");
            let contract = module.contract();
            let roles = contract.embedded_roles(&bytes).expect("embedded roles");
            let artifact = contract
                .component_artifact(&bytes, "codec", 1, &roles)
                .expect("artifact");
            engine.load(&artifact, Install::default().requires(GATE, "gate", 1))
        })
    } else {
        Rc::new(|| {
            provider::orb_named(
                "codec",
                1,
                provider::Providers {
                    gate: Provider::new("gate", 1),
                },
            )
        })
    };
    let gate = Rc::new(Gate::default());
    let contract = Contract::from_source_in(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/native-fixture/wit"),
        "provider.wit",
        provider::WIT,
        Some("provider"),
    )
    .expect("provider contract");
    let (_, signature, _) = contract
        .import_requirement(GATE, None)
        .expect("gate import");
    let pending = gate.clone();
    let gate_orb = Orb::new("gate", 1).service(Service::new(format!("gate::{GATE}"), |_| Ok(()))
        .signature(Some(signature))
        .method("wait", move |_, context, _: Vec<Value>| {
            let gate = pending.clone();
            async move {
                let (send, receive) = oneshot::channel();
                gate.waiters.borrow_mut().push(send);
                gate.started.set(gate.started.get() + 1);
                let cancelled = context.signal();
                let receive = receive.fuse();
                let cancellation = cancelled.cancelled().fuse();
                futures::pin_mut!(receive, cancellation);
                futures::select! {
                    value = receive => { value.map_err(|_| tembra::Error::invalid("gate", "gate closed"))?; },
                    () = cancellation => {
                        gate.cancelled.set(gate.cancelled.get() + 1);
                        let _ = receive.await;
                    },
                }
                // Cooperative completion still settles the cancelled caller physically.
                Ok(1_u32)
            }
        }));
    let (shutdown, stopped) = oneshot::channel();
    let shutdown = RefCell::new(Some(shutdown));
    let control_host = host.clone();
    let reload_host = host.clone();
    let reopen = gate.clone();
    let stats = gate.clone();
    let replace = create.clone();
    let control = Orb::new("control", 1).service(Service::new("control", |_| Ok(()))
        .method("open", move |_, _, (): ()| {
            let pending = std::mem::take(&mut *reopen.waiters.borrow_mut());
            async move { for waiter in pending { let _ = waiter.send(()); } Ok(()) }
        })
        .method("stats", move |_, _, (): ()| {
            let diagnostics = control_host.kernel().diagnostics();
            let started = stats.started.get();
            let cancelled = stats.cancelled.get();
            async move { Ok(json!({"started": started, "cancelled": cancelled, "references": diagnostics.references, "calls": diagnostics.calls})) }
        })
        .method("close", move |_, _, (): ()| {
            if let Some(shutdown) = shutdown.borrow_mut().take() { let _ = shutdown.send(()); }
            async { Ok(()) }
        })
        .method("reload", move |_, _, (): ()| {
            let host = reload_host.clone();
            let replacement = replace();
            async move { host.reload(vec![replacement?], 5000).await }
        }));
    host.install(vec![
        gate_orb,
        create()?,
        control,
        shapes::pace(),
        shapes::provider(mode == "wasm")?,
    ])?;
    let mut options = rpc::Options::new("rpc-reader".into(), |service, _| {
        service == format!("codec::{CODEC}")
            || service == "control"
            || service == format!("shapes::{}", shapes::INTERFACE)
    });
    if let Ok(value) = std::env::var("TEMBRA_RPC_MAX_REFERENCES") {
        options.max_references = value.parse().expect("reference limit");
    }
    if let Ok(value) = std::env::var("TEMBRA_RPC_MAX_FRAME") {
        options.max_frame = value.parse().expect("frame limit");
    }
    transport::run(&mut pool, host, options, stopped, move || {
        let pending = std::mem::take(&mut *gate.waiters.borrow_mut());
        for waiter in pending {
            let _ = waiter.send(());
        }
    })
}

/// Only test control owns these gates; providers await an ordinary imported service.
#[derive(Default)]
struct Gate {
    waiters: RefCell<Vec<oneshot::Sender<()>>>,
    started: Cell<u32>,
    cancelled: Cell<u32>,
}
