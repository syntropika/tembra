//! The same consumer source retains real async suspension, cancellation and owned transfers.
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
};
use tembra::{Context, Host, Orb, Provider, Service};
use tembra_contracts::Contract;
use tembra_executor_wasm::{Engine, Install, Limits};
use tembra_orb_native_fixture::{rpc_consumer::consumer, rpc_provider::provider};

const CODEC: &str = "example:rpc-resources/codec@1.0.0";
const GATE: &str = "example:rpc-resources/gate@1.0.0";

/// Only the host controls physical settlement; cancellation remains cooperative.
#[derive(Default)]
struct Gate {
    started: Cell<bool>,
    cancelled: Cell<bool>,
    release: RefCell<Option<oneshot::Sender<()>>>,
    entered: RefCell<Option<oneshot::Sender<()>>>,
    stopped: RefCell<Option<oneshot::Sender<()>>>,
}

/// Provide the fixed asynchronous contract without adding a consumer-side executor.
fn gate(state: Rc<Gate>) -> Orb {
    let contract = Contract::from_source_in(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/native-fixture/wit"),
        "provider.wit",
        provider::WIT,
        Some("provider"),
    )
    .unwrap();
    let (_, signature, _) = contract.import_requirement(GATE, None).unwrap();
    Orb::new("gate", 1).service(
        Service::new(format!("gate::{GATE}"), |_| Ok(()))
            .signature(Some(signature))
            .method("wait", move |_, context: Context, _: Vec<u32>| {
                let state = state.clone();
                let (release, held) = oneshot::channel();
                state.release.replace(Some(release));
                state.started.set(true);
                state.entered.borrow_mut().take().unwrap().send(()).unwrap();
                async move {
                    let cancelled = context.signal();
                    let stopped = cancelled.cancelled().boxed_local();
                    match futures::future::select(held, stopped).await {
                        futures::future::Either::Left((_, _)) => {}
                        futures::future::Either::Right((_, held)) => {
                            state.cancelled.set(true);
                            state.stopped.borrow_mut().take().unwrap().send(()).unwrap();
                            let _ = held.await;
                        }
                    }
                    Ok(1_u32)
                }
            }),
    )
}

/// Run all four native/component combinations against the same macro-generated contracts.
#[test]
#[ignore = "requires components built by just build-import-resources"]
fn uniform_imports_move_owners_suspend_and_cancel_in_both_builds() {
    for native_provider in [true, false] {
        for native_consumer in [true, false] {
            for cancelled in [false, true] {
                exercise(native_provider, native_consumer, cancelled);
            }
        }
    }
}

/// Exercise a physically pending resource receiver through a real asynchronous WIT import.
fn exercise(native_provider: bool, native_consumer: bool, cancelled: bool) {
    let engine = Engine::new(Limits::default()).unwrap();
    let exporter = if native_provider {
        provider::orb_named(
            "codec",
            1,
            provider::Providers {
                gate: Provider::new("gate", 1),
            },
        )
        .unwrap()
    } else {
        super::compiled(
            &engine,
            "TEMBRA_WASM_IMPORT_PROVIDER",
            "codec",
            Install::default().requires(GATE, "gate", 1),
        )
    };
    let importer = if native_consumer {
        consumer::orb_named(
            "pipeline",
            1,
            consumer::Providers {
                codec: Provider::new("codec", 1),
            },
        )
        .unwrap()
    } else {
        super::compiled(
            &engine,
            "TEMBRA_WASM_IMPORT_CONSUMER",
            "pipeline",
            Install::default().requires(CODEC, "codec", 1),
        )
    };
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (entered, arrived) = oneshot::channel();
    let (stopped, cancelled_import) = oneshot::channel();
    let state = Rc::new(Gate {
        entered: RefCell::new(Some(entered)),
        stopped: RefCell::new(Some(stopped)),
        ..Gate::default()
    });
    host.install(vec![gate(state.clone()), exporter, importer])
        .unwrap();
    let reference = Rc::new(
        host.get(
            "pipeline::example:rpc-consumer/pipeline@1.0.0",
            vec!["main".into()],
            "host",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    let composed = host
        .spawn_root("host", move |context| {
            async move {
                called
                    .call_portable::<_, u64>(&context, "compose", (10_u64,))
                    .await
            }
            .boxed_local()
        })
        .unwrap();
    assert_eq!(pool.run_until(composed).unwrap().unwrap(), 46);
    pool.run_until(host.settled());
    let baseline = host.kernel().diagnostics().references;

    let root = Rc::new(RefCell::new(None));
    let saved = root.clone();
    let called = reference.clone();
    let mut result = host
        .spawn_root("host", move |context| {
            async move {
                saved.replace(Some(context.clone()));
                called
                    .call_portable::<_, u64>(&context, "advance", (19_u64,))
                    .await
            }
            .boxed_local()
        })
        .unwrap();
    match pool.run_until(futures::future::select(arrived, &mut result)) {
        futures::future::Either::Left((arrived, _)) => arrived.unwrap(),
        futures::future::Either::Right((result, _)) => {
            panic!("call ended before reaching its async import: {result:?}")
        }
    }
    assert!(
        state.started.get(),
        "native provider: {native_provider}, native consumer: {native_consumer}"
    );
    assert!(
        result.try_recv().unwrap().is_none(),
        "the async import must actually suspend"
    );
    if cancelled {
        root.borrow().as_ref().unwrap().cancel().unwrap();
        pool.run_until(cancelled_import).unwrap();
        pool.run_until_stalled();
        assert!(
            state.cancelled.get(),
            "cancellation must reach the admitted import"
        );
    }
    assert!(
        host.kernel().diagnostics().references > baseline,
        "physical work must retain its resource owner"
    );
    state.release.borrow_mut().take().unwrap().send(()).unwrap();
    let result = pool.run_until(result).unwrap();
    if cancelled {
        // Reference cancellation is cooperative: a provider can finish the admitted call.
        match result {
            Ok(value) => assert_eq!(value, 20),
            Err(error) => assert_eq!(error.code, tembra::Code::Cancelled),
        }
    } else {
        assert_eq!(result.unwrap(), 20);
    }
    pool.run_until(host.settled());
    assert_eq!(
        host.kernel().diagnostics().references,
        baseline,
        "resource claims must settle exactly once"
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (diagnostics.references, diagnostics.calls, diagnostics.roots),
        (0, 0, 0)
    );
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(engine.instances(), 0);
}
