//! Compiled WIT resources across the native capability boundary.
//!
//! The fixtures are real components built from `rust-resource-provider` and
//! `rust-resource-consumer` against `tests/wit/resources`, supplied as
//! `TEMBRA_WASM_RESOURCE_PROVIDER` and `TEMBRA_WASM_RESOURCE_CONSUMER`. Every case runs on
//! Cranelift and on Pulley.
//!
//! Exported instances stay in their provider's Store. Native owners hold `Owned` capabilities
//! whose calls pass ordinary kernel admission through their own claim. A compiled importer holds
//! them as dynamic handles naming its own table slots.
use futures::{
    FutureExt,
    executor::LocalPool,
    future::{AbortHandle, Abortable},
    task::LocalSpawnExt,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use tembra::{
    AcquisitionOptions, Context, Host, Orb, Reference, Service,
    portable::{Delivered, Owned, Payload},
};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const SESSIONS: &str = "tembra-test:resources/sessions@1.0.0";
const VIEWS: &str = "tembra-test:resources/views@1.0.0";
const PACE: &str = "tembra-test:resources/pace@1.0.0";
const PACED: &str = "[method]session.paced-add";
const FRAGILE: &str = "[constructor]fragile";
const RELAY: &str = "tembra-test:resources/relay@1.0.0";
const CONSTRUCT: &str = "[constructor]session";
const ADD: &str = "[method]session.add";
const TOTAL: &str = "[method]session.total";
const COUNTER: &str = "[constructor]counter";
const HITS: &str = "[method]counter.hits";
const OPEN_KEYED: &str = "open-keyed";
const KEY: &str = "[method]session.key";
const OPEN_LATER: &str = "open-later";

/// Typed session keys crossing the real WIT boundary: astral and combining characters, quotes,
/// a backslash and a slash must arrive unchanged.
const FIRST_KEY: (&str, &str) = ("tenant ✓ ключ", "decoder/🎧 \"h264\" \\ e\u{301}");
const SECOND_KEY: (&str, &str) = ("b", "y");
/// A seed above 2^53: totals must stay exact 64-bit integers.
const KEYED_SEED: u64 = u64::MAX - 10;

/// One real fixture component, or `None` when the variable is unset (reported as a skip).
fn fixture(variable: &str) -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os(variable).filter(|path| !path.is_empty()) else {
        eprintln!("skipped: {variable} names no compiled resource fixture");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn contract(world: &str) -> Contract {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/resources"),
        Some(world),
    )
    .unwrap()
}

fn engine(interpreted: bool) -> Engine {
    let limits = Limits {
        timeout_ms: 30_000,
        ..Limits::default()
    };
    if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
}

fn provider(engine: &Engine, bytes: &[u8], name: &str) -> Orb {
    engine
        .load(
            &contract("provider")
                .component_artifact(bytes, name, 1, &Default::default())
                .unwrap(),
            Install::default().requires(PACE, "pace", 1),
        )
        .unwrap()
}

/// The native `pace` provider's observable state: a gate for `wait` and call counts.
#[derive(Default)]
struct Pace {
    gate: RefCell<Option<futures::channel::oneshot::Receiver<()>>>,
    waits: Cell<u32>,
    notes: Cell<u32>,
}

/// A native Orb serving `pace`. `wait` answers `n * 10`, after the gate opens if one is set.
fn pacer(pace: Rc<Pace>) -> Orb {
    let service = contract("pacer")
        .plan("pace", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("pace", 1).service(
        Service::new(service.name, move |_| Ok(pace.clone()))
            .signature(service.signature)
            .method("wait", |pace: Rc<Rc<Pace>>, _, (n,): (u64,)| {
                pace.waits.set(pace.waits.get() + 1);
                let gate = pace.gate.borrow_mut().take();
                async move {
                    if let Some(gate) = gate {
                        let _ = gate.await;
                    }
                    Ok(n * 10)
                }
            })
            .method("note", |pace: Rc<Rc<Pace>>, _, (_n,): (u64,)| {
                pace.notes.set(pace.notes.get() + 1);
                async move { Ok(()) }
            }),
    )
}

fn consumer(engine: &Engine, bytes: &[u8], name: &str, provider: &str) -> Orb {
    engine
        .load(
            &contract("consumer")
                .component_artifact(bytes, name, 1, &Default::default())
                .unwrap(),
            Install::default()
                .requires(SESSIONS, provider, 1)
                .requires(VIEWS, provider, 1),
        )
        .unwrap()
}

fn sessions(provider: &str) -> String {
    format!("{provider}::{SESSIONS}")
}

/// One live reference to `service`'s main object for `principal`.
fn reference(pool: &mut LocalPool, host: &Host, service: &str, principal: &str) -> Rc<Reference> {
    let reference = Rc::new(host.get(service, vec!["main".into()], principal).unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Run one operation under a fresh root.
fn root<T: 'static>(
    pool: &mut LocalPool,
    host: &Host,
    operation: impl AsyncFnOnce(Context) -> tembra::Result<T> + 'static,
) -> tembra::Result<T> {
    pool.run_until(host.root("alice", move |context| operation(context).boxed_local()))
}

/// Read the provider Store's destructor count. The Store serves commands in order, so every
/// destruction requested before this call has run.
fn closed(pool: &mut LocalPool, host: &Host, reference: &Rc<Reference>) -> u64 {
    let reference = reference.clone();
    root(pool, host, async move |context| {
        reference
            .call_portable::<_, u64>(&context, "closed", json!([]))
            .await
    })
    .unwrap()
}

/// Generate a Cranelift and a Pulley test for one scenario.
macro_rules! both_engines {
    ($($scenario:ident),* $(,)?) => {
        mod cranelift {
            $(
                #[test]
                #[ignore = "requires TEMBRA_WASM_RESOURCE_PROVIDER and TEMBRA_WASM_RESOURCE_CONSUMER"]
                fn $scenario() {
                    super::$scenario(false);
                }
            )*
        }
        mod pulley {
            $(
                #[test]
                #[ignore = "requires TEMBRA_WASM_RESOURCE_PROVIDER and TEMBRA_WASM_RESOURCE_CONSUMER"]
                fn $scenario() {
                    super::$scenario(true);
                }
            )*
        }
    };
}

both_engines!(
    native_owners_get_fresh_independent_instances,
    owned_handles_outlive_their_reference_and_root_with_its_admission,
    forged_and_mistyped_receivers_are_refused_before_lowering,
    a_compiled_importer_constructs_borrows_keeps_and_drops,
    undelivered_and_refused_results_are_destroyed_once,
    reload_never_moves_an_instance_to_the_new_store,
    host_close_drains_live_instances,
    platform_ports_exchange_data_only,
    ingress_releases_dropped_imports_on_completion,
    async_methods_keep_their_receiver_through_suspension,
    a_failed_importer_reply_closes_the_late_capability,
    an_aliasing_importer_installs_and_calls,
    destructor_imports_quarantine_the_revision,
    a_failed_close_drain_is_reported,
    a_guest_global_handle_crosses_keyed_objects,
    idle_destruction_releases_the_destructors_imports,
    an_abandoned_async_import_closes_its_late_result,
    an_abandoned_started_import_is_joined_and_closes_its_late_result,
    equal_keys_open_independent_native_sessions,
    a_compiled_importer_opens_independent_keyed_sessions,
    one_revision_serves_its_sessions_one_call_at_a_time,
);

/// Fresh constructions with equal arguments, factory results, borrowed mutation and drops.
fn native_owners_get_fresh_independent_instances(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let (first, second, made, totals) = root(&mut pool, &host, async move |context| {
        let first = called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (5_u64,))
            .await?;
        let second = called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (5_u64,))
            .await?;
        let made = called
            .call_portable::<_, Owned>(&context, "open", (9_u64,))
            .await?;
        let totals = (
            first
                .call_portable::<_, u64>(&context, ADD, (3_u64,))
                .await?,
            first
                .call_portable::<_, u64>(&context, TOTAL, json!([]))
                .await?,
            second
                .call_portable::<_, u64>(&context, TOTAL, json!([]))
                .await?,
            made.call_portable::<_, u64>(&context, ADD, (1_u64,))
                .await?,
            // Data functions keep the typed native path and the portable one.
            called
                .call::<_, Value>(&context, "label", json!([]))
                .await?,
            called
                .call_portable::<_, String>(&context, "label", json!([]))
                .await?,
        );
        Ok((first, second, made, totals))
    })
    .unwrap();
    assert_eq!(
        totals,
        (8, 8, 5, 10, json!("sessions"), "sessions".to_owned())
    );
    assert_eq!(engine.resources(), 3);
    assert_eq!(closed(&mut pool, &host, &store), 0);
    drop(first);
    assert_eq!(closed(&mut pool, &host, &store), 1);
    // The other instances are untouched.
    let called = second;
    let total = root(&mut pool, &host, async move |context| {
        let total = called
            .call_portable::<_, u64>(&context, TOTAL, json!([]))
            .await?;
        Ok((total, called))
    })
    .unwrap();
    assert_eq!(total.0, 5);
    drop(total.1);
    drop(made);
    assert_eq!(closed(&mut pool, &host, &store), 3);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// An owned handle keeps exactly its constructor reference's admission, beyond that
/// reference and its root.
fn owned_handles_outlive_their_reference_and_root_with_its_admission(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let service = sessions("store");
    let session = root(&mut pool, &host, async move |context| {
        // A root-scoped claim requiring only the constructor and `add`.
        let narrowed = context.get(
            &service,
            vec!["main".into()],
            AcquisitionOptions {
                api_version: 1,
                methods: BTreeSet::from([CONSTRUCT.to_owned(), ADD.to_owned()]),
            },
        )?;
        narrowed.ready().await?;
        narrowed
            .call_portable::<_, Owned>(&context, CONSTRUCT, (2_u64,))
            .await
    })
    .unwrap();
    // The root ended and revoked its claim; the handle has its own.
    let (added, refused, session) = root(&mut pool, &host, async move |context| {
        let added = session
            .call_portable::<_, u64>(&context, ADD, (40_u64,))
            .await?;
        let refused = session
            .call_portable::<_, u64>(&context, TOTAL, json!([]))
            .await
            .unwrap_err();
        Ok((added, refused, session))
    })
    .unwrap();
    assert_eq!(added, 42);
    assert_eq!(refused.code, tembra::Code::Forbidden, "{refused}");
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    drop(session);
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// No call without a live receiver of exactly the method's type reaches guest lowering.
fn forged_and_mistyped_receivers_are_refused_before_lowering(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let (errors, session, counter) = root(&mut pool, &host, async move |context| {
        let session = called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (7_u64,))
            .await?;
        let counter = called
            .call_portable::<_, Owned>(&context, COUNTER, json!([]))
            .await?;
        let errors = vec![
            // A method through the service reference has no `self`.
            called
                .call_portable::<_, u64>(&context, ADD, (1_u64,))
                .await
                .unwrap_err(),
            // A plain number in the data cannot stand for `self`.
            called
                .call_portable::<_, u64>(&context, ADD, (1_u32, 1_u64))
                .await
                .unwrap_err(),
            // A handle of another resource type.
            counter
                .call_portable::<_, u64>(&context, ADD, (1_u64,))
                .await
                .unwrap_err(),
            session
                .call_portable::<_, u64>(&context, HITS, json!([]))
                .await
                .unwrap_err(),
            // Data where the export takes no receiver.
            session
                .call_portable::<_, String>(&context, "label", json!([]))
                .await
                .unwrap_err(),
        ];
        Ok((errors, session, counter))
    })
    .unwrap();
    // Refusals of the call itself, never a Store fault.
    let codes: Vec<_> = errors
        .iter()
        .map(|error| (error.code, error.reason.as_str()))
        .collect();
    assert_eq!(
        codes,
        [
            (tembra::Code::Invalid, "resource"),
            (tembra::Code::Invalid, "resource"),
            (tembra::Code::Forbidden, "resource"),
            (tembra::Code::Forbidden, "resource"),
            (tembra::Code::Invalid, "local_value"),
        ],
        "{errors:?}"
    );
    // The Store is healthy and the instances are untouched.
    let (total, hits, session, counter) = root(&mut pool, &host, async move |context| {
        let total = session
            .call_portable::<_, u64>(&context, TOTAL, json!([]))
            .await?;
        let hits = counter
            .call_portable::<_, u64>(&context, HITS, json!([]))
            .await?;
        Ok((total, hits, session, counter))
    })
    .unwrap();
    assert_eq!((total, hits), (7, 1));
    drop((session, counter));
    // Only sessions count; the counter has no destructor side effect.
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A compiled importer uses an independent compiled provider's resources only through handles.
fn a_compiled_importer_constructs_borrows_keeps_and_drops(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &provided, "store"),
        provider(&engine, &provided, "other"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    assert_eq!(engine.instances(), 3);
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let other = reference(&mut pool, &host, &sessions("other"), "alice");
    let call = |pool: &mut LocalPool, method: &'static str, args: Value| {
        let relay = relay.clone();
        root(pool, &host, async move |context| {
            relay.call::<_, Value>(&context, method, args).await
        })
    };
    // Two equal constructions and one factory result, all dropped when `pair` returns.
    assert_eq!(
        call(&mut pool, "pair", json!([4, 2])).unwrap(),
        json!([6, 4, 3])
    );
    assert_eq!(call(&mut pool, "closed", json!([])).unwrap(), json!(3));
    // One owned session kept across calls and roots.
    assert_eq!(call(&mut pool, "keep", json!([10])).unwrap(), json!(10));
    assert_eq!(call(&mut pool, "bump", json!([5])).unwrap(), json!(15));
    assert_eq!(call(&mut pool, "bump", json!([1])).unwrap(), json!(16));
    assert_eq!(engine.resources(), 1);
    assert_eq!(call(&mut pool, "closed", json!([])).unwrap(), json!(3));
    call(&mut pool, "discard", json!([])).unwrap();
    assert_eq!(call(&mut pool, "closed", json!([])).unwrap(), json!(4));
    assert_eq!(engine.resources(), 0);
    // The independent provider was never touched.
    assert_eq!(closed(&mut pool, &host, &other), 0);
    // A kept handle is released with its importer's environment.
    assert_eq!(call(&mut pool, "keep", json!([1])).unwrap(), json!(1));
    assert_eq!(engine.resources(), 1);
    relay.release().unwrap();
    other.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.resources(), 0);
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A result that never reaches its caller, or that the caller refuses, is destroyed exactly once.
fn undelivered_and_refused_results_are_destroyed_once(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    // A data-only result type refuses the capability, which closes it.
    let called = store.clone();
    let refused = root(&mut pool, &host, async move |context| {
        called
            .call_portable::<_, u64>(&context, CONSTRUCT, (1_u64,))
            .await
    })
    .unwrap_err();
    assert_eq!(refused.reason, "local_value", "{refused}");
    assert_eq!(closed(&mut pool, &host, &store), 1);
    // An executor-style caller that drops what it received.
    let called = store.clone();
    let delivered = root(&mut pool, &host, async move |context| {
        called
            .call_portable::<_, Delivered>(&context, CONSTRUCT, Payload::data(json!([2])))
            .await
    })
    .unwrap();
    assert_eq!(delivered.owned.len(), 1);
    drop(delivered);
    assert_eq!(closed(&mut pool, &host, &store), 2);
    // A caller abandoned after the Store issued the instance. Keep any result that arrives
    // during polling live until abandonment, so fast delivery cannot erase the observation.
    let (abort, registration) = AbortHandle::new_pair();
    let called = store.clone();
    let owner = host.clone();
    let caller = async move {
        owner
            .root("alice", move |context| {
                async move {
                    let owned = called
                        .call_portable::<_, Owned>(&context, CONSTRUCT, (3_u64,))
                        .await?;
                    futures::future::pending::<()>().await;
                    drop(owned);
                    Ok(())
                }
                .boxed_local()
            })
            .await
    };
    pool.spawner()
        .spawn_local(Abortable::new(caller, registration).map(|outcome| {
            if let Ok(result) = outcome {
                panic!("caller completed before abandonment: {result:?}");
            }
        }))
        .unwrap();
    let started = std::time::Instant::now();
    while engine.resources() == 0 {
        pool.run_until_stalled();
        assert!(started.elapsed() < Duration::from_secs(20), "never issued");
        std::thread::sleep(Duration::from_millis(1));
    }
    abort.abort();
    pool.run_until(host.settled());
    assert_eq!(closed(&mut pool, &host, &store), 3);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// After a reload the old instance is never routed to, or re-created in, the new Store.
fn reload_never_moves_an_instance_to_the_new_store(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let session = root(&mut pool, &host, async move |context| {
        called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (5_u64,))
            .await
    })
    .unwrap();
    pool.run_until(host.reload(vec![provider(&engine, &bytes, "store")], 5000))
        .unwrap();
    pool.run_until(host.settled());
    let (stale, session) = root(&mut pool, &host, async move |context| {
        let stale = session
            .call_portable::<_, u64>(&context, ADD, (1_u64,))
            .await
            .unwrap_err();
        Ok((stale, session))
    })
    .unwrap();
    assert_eq!(stale.code, tembra::Code::Gone, "{stale}");
    // The old revision retired once it settled; its instance left the old table with it.
    assert_eq!(engine.instances(), 1);
    assert_eq!(engine.resources(), 0);
    // The new Store issues its own instances and destroyed none.
    let called = store.clone();
    let fresh = root(&mut pool, &host, async move |context| {
        let fresh = called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (8_u64,))
            .await?;
        let total = fresh
            .call_portable::<_, u64>(&context, TOTAL, json!([]))
            .await?;
        Ok((total, fresh))
    })
    .unwrap();
    assert_eq!(fresh.0, 8);
    assert_eq!(closed(&mut pool, &host, &store), 0);
    // The stale owner's issuer is gone, so dropping it requests nothing.
    drop(session);
    assert_eq!(closed(&mut pool, &host, &store), 0);
    drop(fresh.1);
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.resources(), 0);
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// Closing the host destroys instances whose native owners are still alive.
fn host_close_drains_live_instances(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let owners = root(&mut pool, &host, async move |context| {
        let mut owners = Vec::new();
        for seed in 0..3_u64 {
            owners.push(
                called
                    .call_portable::<_, Owned>(&context, CONSTRUCT, (seed,))
                    .await?,
            );
        }
        Ok(owners)
    })
    .unwrap();
    assert_eq!(engine.resources(), 3);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.resources(), 0);
    assert_eq!(engine.instances(), 0);
    // Owners outliving the host close nothing twice and report no error.
    drop(owners);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// Platform ports carry JSON data only: a resource-bearing artifact is refused before any Store
/// starts, with a diagnostic naming the missing transport.
fn platform_ports_exchange_data_only(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let exporter = contract("provider")
        .component_artifact(&provided, "store", 1, &Default::default())
        .unwrap();
    let importer = contract("consumer")
        .component_artifact(&consuming, "relay", 1, &Default::default())
        .unwrap();
    let select = |artifact: &tembra_contracts::Artifact, imports: &[&str]| {
        let requires = imports
            .iter()
            .map(|import| {
                (
                    (*import).to_owned(),
                    tembra_contracts::OrbProvider {
                        orb: "store".into(),
                        api_version: 1,
                    },
                )
            })
            .collect();
        artifact
            .contract
            .providers(&requires, &Default::default())
            .unwrap()
    };
    for (artifact, imports) in [
        (&exporter, &[PACE][..]),
        (&importer, &[SESSIONS, VIEWS][..]),
    ] {
        let artifact = tembra_contracts::Artifact::decode(artifact).unwrap();
        let bindings = select(&artifact, imports);
        let error = engine
            .open_port(
                &artifact,
                &bindings,
                &Default::default(),
                &Default::default(),
            )
            .err()
            .expect("a platform port refuses WIT resources");
        assert!(error.message.contains("native Rust facade"), "{error}");
    }
    assert_eq!(engine.instances(), 0);
}

/// Diagnostics an engine reports, as `level: message`.
type Logs = Arc<Mutex<Vec<String>>>;

fn logged(interpreted: bool) -> (Engine, Logs) {
    let logs = Logs::default();
    let sink = logs.clone();
    let engine = engine(interpreted).on_log(move |record| {
        sink.lock()
            .unwrap()
            .push(format!("{}: {}", record.level, record.message));
    });
    (engine, logs)
}

/// Run `operation` until `ready` holds, sleeping between polls of the local pool.
fn until(pool: &mut LocalPool, ready: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while !ready() {
        pool.run_until_stalled();
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "condition never held"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A handler's dropped import is released when its request completes, not at a later call.
fn ingress_releases_dropped_imports_on_completion(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let handler = reference(
        &mut pool,
        &host,
        &tembra_executor_wasm::ingress::handler_service("relay"),
        "alice",
    );
    let request = http::Request::get("http://relay.test/")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();
    let (response, settled) = pool
        .run_until(tembra_executor_wasm::ingress::serve(
            &host,
            "alice",
            handler.clone(),
            request,
        ))
        .unwrap();
    assert_eq!(response.headers()["x-total"], "8");
    drop(response);
    pool.run_until(settled).unwrap();
    // No later call of the importer: the request's own completion released its drop.
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    handler.release().unwrap();
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// An async method that suspends while borrowing `self` keeps its instance alive until the
/// call has physically finished, for native and compiled callers.
fn async_methods_keep_their_receiver_through_suspension(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let pace = Rc::new(Pace::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(pace.clone()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    // A native owner: the method awaits the host while borrowing its instance.
    let called = store.clone();
    let (session, paced) = root(&mut pool, &host, async move |context| {
        let session = called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (5_u64,))
            .await?;
        let paced = session
            .call_portable::<_, u64>(&context, PACED, (2_u64,))
            .await?;
        Ok((session, paced))
    })
    .unwrap();
    assert_eq!(paced, 25);
    assert_eq!(pace.waits.get(), 1);
    // The caller, which owns the only handle, is abandoned while the borrow is suspended.
    let (open, gate) = futures::channel::oneshot::channel();
    *pace.gate.borrow_mut() = Some(gate);
    let (abort, registration) = AbortHandle::new_pair();
    let owner = host.clone();
    let caller = async move {
        owner
            .root("alice", move |context| {
                async move {
                    session
                        .call_portable::<_, u64>(&context, PACED, (1_u64,))
                        .await
                        .map(drop)
                }
                .boxed_local()
            })
            .await
    };
    pool.spawner()
        .spawn_local(Abortable::new(caller, registration).map(drop))
        .unwrap();
    until(&mut pool, || pace.waits.get() == 2);
    abort.abort();
    pool.run_until_stalled();
    // The owner is gone, but the suspended call still pins the instance.
    assert_eq!(engine.resources(), 1);
    open.send(()).unwrap();
    pool.run_until(host.settled());
    // Destroyed once, after the call physically finished.
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    // A compiled importer awaiting an async method of a session it owns.
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let called = relay.clone();
    let paced = root(&mut pool, &host, async move |context| {
        called
            .call::<_, Value>(&context, "paced", json!([4, 2]))
            .await
    })
    .unwrap();
    assert_eq!(paced, json!(24));
    assert_eq!(pace.waits.get(), 3);
    assert_eq!(closed(&mut pool, &host, &store), 2);
    assert_eq!(engine.resources(), 0);
    relay.release().unwrap();
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A test capability whose closes are counted; issued by a controlled native provider.
struct Late {
    nominal: tembra::portable::Nominal,
    store: Rc<LateStore>,
}

impl tembra::portable::Capability for Late {
    fn nominal(&self) -> &tembra::portable::Nominal {
        &self.nominal
    }
}

impl Drop for Late {
    fn drop(&mut self) {
        self.store.closed.set(self.store.closed.get() + 1);
    }
}

/// The observable state of the controlled native `store` built by [`late_store`].
#[derive(Default)]
struct LateStore {
    /// Held by the next constructor or `open-later` call, which answers once it opens.
    gate: RefCell<Option<futures::channel::oneshot::Receiver<()>>>,
    /// Counted capabilities closed.
    closed: Cell<u32>,
    /// Fired when the next `open-later` call starts running.
    started: RefCell<Option<futures::channel::oneshot::Sender<()>>>,
    /// Awaited by the next `closed` call: it answers only once `open-later` started.
    checkpoint: RefCell<Option<futures::channel::oneshot::Receiver<()>>>,
    /// The latest `open-later` call's own signal.
    signal: RefCell<Option<tembra::Cancellation>>,
    /// `closed` calls answered.
    answered: Cell<u32>,
    /// Ordered events of both sides.
    log: RefCell<Vec<&'static str>>,
}

impl LateStore {
    /// Hold the next `open-later` call, and make the next `closed` call its checkpoint.
    /// Returns the sender that opens the gate.
    fn hold_open_later(&self) -> futures::channel::oneshot::Sender<()> {
        let (open, gate) = futures::channel::oneshot::channel();
        let (started, checkpoint) = futures::channel::oneshot::channel();
        *self.gate.borrow_mut() = Some(gate);
        *self.started.borrow_mut() = Some(started);
        *self.checkpoint.borrow_mut() = Some(checkpoint);
        open
    }

    fn note(&self, event: &'static str) {
        self.log.borrow_mut().push(event);
    }

    fn position(&self, event: &str) -> Option<usize> {
        self.log.borrow().iter().position(|logged| *logged == event)
    }

    /// One counted session capability as the call's result.
    fn issue(self: &Rc<Self>) -> tembra::Result<Payload> {
        let mut result = Payload::data(Value::Null);
        let index = result.attach(tembra::portable::Issued::new(Late {
            nominal: tembra::portable::Nominal {
                interface: SESSIONS.into(),
                name: "session".into(),
            },
            store: self.clone(),
        }));
        result.map_value(|_| Ok(json!(index)))
    }
}

/// A controlled native `store` for the importer's late-result paths only. It is not a native
/// resource provider: the session constructor and `open-later` answer with a counted capability
/// once their gate opens, even when their call was cancelled; `closed` answers the count of
/// closed capabilities, after its checkpoint if one is set. Other methods are unsupported.
fn late_store(store: Rc<LateStore>) -> Orb {
    let pace = std::collections::BTreeMap::from([(
        PACE.to_owned(),
        tembra_executor_wasm::Binding::Orb {
            name: "pace".into(),
            api_version: 1,
        },
    )]);
    let plan = contract("provider").plan("store", 1, &pace).unwrap();
    let mut orb = Orb::new("store", 1);
    for service in plan.orb.services {
        let methods: Vec<String> = service.methods.iter().cloned().collect();
        let mut native = Service::new(service.name, |_| Ok(())).signature(service.signature);
        for method in methods {
            let store = store.clone();
            native = match method.as_str() {
                CONSTRUCT => native.payload_method(method, move |_, _, payload: Payload| {
                    let gate = store.gate.borrow_mut().take();
                    let store = store.clone();
                    async move {
                        payload.into_data()?;
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        store.issue()
                    }
                }),
                OPEN_LATER => native.payload_method(method, move |_, context: Context, payload| {
                    let store = store.clone();
                    async move {
                        payload.into_data()?;
                        store.note("provider:start");
                        *store.signal.borrow_mut() = Some(context.signal());
                        if let Some(started) = store.started.borrow_mut().take() {
                            let _ = started.send(());
                        }
                        let gate = store.gate.borrow_mut().take();
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        // Answer even when cancelled: the owned result arrives late.
                        store.note("provider:end");
                        store.issue()
                    }
                }),
                "closed" => native.payload_method(method, move |_, _, payload: Payload| {
                    let store = store.clone();
                    async move {
                        payload.into_data()?;
                        let checkpoint = store.checkpoint.borrow_mut().take();
                        if let Some(checkpoint) = checkpoint {
                            let _ = checkpoint.await;
                        }
                        store.answered.set(store.answered.get() + 1);
                        Ok(Payload::data(json!(store.closed.get())))
                    }
                }),
                _ => native.payload_method(method, |_, _, _: Payload| async move {
                    Err::<Payload, _>(tembra::Error::new(
                        tembra::Code::Unsupported,
                        "test",
                        "not served by this provider",
                    ))
                }),
            };
        }
        orb = orb.service(native);
    }
    orb
}

/// The importer loses its reply while a late capability is still on its way: it is closed
/// exactly once and no slot or admission claim of the importer remains.
fn a_failed_importer_reply_closes_the_late_capability(interpreted: bool) {
    let Some(consuming) = fixture("TEMBRA_WASM_RESOURCE_CONSUMER") else {
        return;
    };
    let limits = Limits {
        timeout_ms: 1_000,
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap();
    let store = Rc::new(LateStore::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        late_store(store.clone()),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let claims = host.kernel().diagnostics().references;
    let (open, late) = futures::channel::oneshot::channel();
    *store.gate.borrow_mut() = Some(late);
    let called = relay.clone();
    let timed_out = root(&mut pool, &host, async move |context| {
        called.call::<_, Value>(&context, "keep", json!([1])).await
    })
    .unwrap_err();
    assert!(timed_out.message.contains("timed out"), "{timed_out}");
    assert_eq!(store.closed.get(), 0);
    // The provider answers after the importer is gone.
    open.send(()).unwrap();
    pool.run_until(host.settled());
    assert_eq!(
        store.closed.get(),
        1,
        "the late capability is closed exactly once"
    );
    assert_eq!(host.kernel().diagnostics().references, claims);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    let called = relay.clone();
    let quarantined = root(&mut pool, &host, async move |context| {
        called.call::<_, Value>(&context, "closed", json!([])).await
    })
    .unwrap_err();
    assert!(quarantined.message.contains("timed out"), "{quarantined}");
    relay.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(store.closed.get(), 1);
    assert_eq!(engine.instances(), 0);
}

/// `views` names `sessions`' resource through an alias. The guest compiler elides that unused
/// alias from this binary (it never uses it), so this proves the importer installs and calls
/// through both interfaces; the retained-alias type check is the `component` unit test.
fn an_aliasing_importer_installs_and_calls(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let call = |pool: &mut LocalPool, method: &'static str, args: Value| {
        let relay = relay.clone();
        root(pool, &host, async move |context| {
            relay.call::<_, Value>(&context, method, args).await
        })
    };
    assert_eq!(
        call(&mut pool, "describe", json!([])).unwrap(),
        json!("views")
    );
    assert_eq!(call(&mut pool, "keep", json!([2])).unwrap(), json!(2));
    assert_eq!(call(&mut pool, "bump", json!([3])).unwrap(), json!(5));
    relay.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.resources(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A destructor holds no call authority: its import is refused, the revision is quarantined
/// with that cause, and the failure is reported even though no caller awaited it.
fn destructor_imports_quarantine_the_revision(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let (engine, logs) = logged(interpreted);
    let pace = Rc::new(Pace::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(pace.clone()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let fragile = root(&mut pool, &host, async move |context| {
        called
            .call_portable::<_, Owned>(&context, FRAGILE, json!([]))
            .await
    })
    .unwrap();
    drop(fragile);
    let called = store.clone();
    let refused = root(&mut pool, &host, async move |context| {
        called
            .call_portable::<_, u64>(&context, "closed", json!([]))
            .await
    })
    .unwrap_err();
    assert_eq!(refused.reason, "wasm_executor_quarantined", "{refused}");
    assert!(
        refused.message.contains("no admitted invocation"),
        "{refused}"
    );
    // The import never reached the provider.
    assert_eq!(pace.notes.get(), 0);
    until(&mut pool, || {
        logs.lock().unwrap().iter().any(|log| {
            log.starts_with("error: revision quarantined") && log.contains("no admitted invocation")
        })
    });
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.resources(), 0);
    assert_eq!(engine.instances(), 0);
}

/// A destructor failing while the host closes is reported, not passed off as a clean release.
fn a_failed_close_drain_is_reported(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let (engine, logs) = logged(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let owners = root(&mut pool, &host, async move |context| {
        let session = called
            .call_portable::<_, Owned>(&context, CONSTRUCT, (1_u64,))
            .await?;
        let fragile = called
            .call_portable::<_, Owned>(&context, FRAGILE, json!([]))
            .await?;
        Ok((session, fragile))
    })
    .unwrap();
    assert_eq!(engine.resources(), 2);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    drop(owners);
    assert_eq!(engine.resources(), 0);
    assert_eq!(engine.instances(), 0);
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|log| log.starts_with("error: revision quarantined")
                && log.contains("no admitted invocation")),
        "{logs:?}"
    );
}

/// Ownership rule (G1): an owned resource a compiled importer receives belongs to the Store's
/// revision, because the guest's state is shared by all its objects. A handle kept in guest
/// state is usable through another keyed object, and outlives the object whose call received it.
fn a_guest_global_handle_crosses_keyed_objects(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let object = |pool: &mut LocalPool, key: &str, principal: &str| {
        let reference = Rc::new(
            host.get(&format!("relay::{RELAY}"), vec![key.into()], principal)
                .unwrap(),
        );
        pool.run_until(reference.ready()).unwrap();
        reference
    };
    let call = |pool: &mut LocalPool, reference: &Rc<Reference>, method: &'static str, args| {
        let reference = reference.clone();
        root(pool, &host, async move |context| {
            reference.call::<_, Value>(&context, method, args).await
        })
    };
    let first = object(&mut pool, "first", "alice");
    let second = object(&mut pool, "second", "bob");
    assert_eq!(
        call(&mut pool, &first, "keep", json!([4])).unwrap(),
        json!(4)
    );
    // Another keyed object of the same Store reaches the very same instance.
    assert_eq!(
        call(&mut pool, &second, "bump", json!([1])).unwrap(),
        json!(5)
    );
    // Disposing the object whose call received it does not destroy it.
    first.release().unwrap();
    pool.run_until(host.settled());
    assert_eq!(engine.resources(), 1);
    assert_eq!(
        call(&mut pool, &second, "bump", json!([1])).unwrap(),
        json!(6)
    );
    assert_eq!(closed(&mut pool, &host, &store), 0);
    // The guest's own drop destroys it once.
    call(&mut pool, &second, "discard", json!([])).unwrap();
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    second.release().unwrap();
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// An idle destruction releases the imported resources its guest destructor dropped: the
/// remote instance is destroyed without any further call of either Orb.
fn idle_destruction_releases_the_destructors_imports(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let called = relay.clone();
    let (holder, total) = root(&mut pool, &host, async move |context| {
        let holder = called
            .call_portable::<_, Owned>(&context, "[constructor]holder", (5_u64,))
            .await?;
        let total = holder
            .call_portable::<_, u64>(&context, "[method]holder.total", json!([]))
            .await?;
        Ok((holder, total))
    })
    .unwrap();
    assert_eq!(total, 5);
    // The relay's holder and the store's session it owns.
    assert_eq!(engine.resources(), 2);
    let claims = host.kernel().diagnostics().references;
    drop(holder);
    // The relay's idle destruction ran now and released the session its destructor dropped:
    // the relay's admission claim on the store is gone without any further relay call.
    assert!(host.kernel().diagnostics().references < claims);
    // The store destroys the session in command order; its next call observes it.
    assert_eq!(closed(&mut pool, &host, &store), 1);
    assert_eq!(engine.resources(), 0);
    relay.release().unwrap();
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The guest cancels a pending async import whose result is an owned resource.
///
/// The cancellation reaches the host at once: the abandoned import's dependency call is
/// cancelled (that call only, never the root) and stays owned by the importer's callback until
/// it has physically ended, so the importer's call completes only after the provider's call
/// ended. The cancellation usually reaches the provider's call before it executes (observed on
/// both engines: it never runs); if it ran first, its late owned result has no receiver and is
/// destroyed exactly once. If its nested ordinary import is refused after cancellation, the
/// provider is quarantined. All three outcomes must leave no slot, claim, call or instance. The window after
/// the host committed a slot is covered by the `ImportReply` unit tests; a provider that has
/// definitely started is covered by
/// `an_abandoned_started_import_is_joined_and_closes_its_late_result`.
fn an_abandoned_async_import_closes_its_late_result(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let pace = Rc::new(Pace::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(pace.clone()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let claims = host.kernel().diagnostics().references;
    let (open, gate) = futures::channel::oneshot::channel();
    *pace.gate.borrow_mut() = Some(gate);
    let (release, hold) = futures::channel::oneshot::channel::<()>();
    let answer = Rc::new(RefCell::new(None));
    let (called, answered, owner) = (relay.clone(), answer.clone(), host.clone());
    let finished = pool
        .spawner()
        .spawn_local_with_handle(async move {
            owner
                .root("alice", move |context| {
                    async move {
                        let result = called
                            .call::<_, Value>(&context, "abandon", json!([3]))
                            .await?;
                        *answered.borrow_mut() = Some(result);
                        // Keep the root admitted until the provider has settled.
                        let _ = hold.await;
                        Ok(())
                    }
                    .boxed_local()
                })
                .await
        })
        .unwrap();
    // The provider is never held: whether it ran or was cancelled first, the importer's call
    // completes only once the provider's call has physically ended.
    open.send(()).unwrap();
    until(&mut pool, || answer.borrow().is_some());
    assert_eq!(
        answer.borrow_mut().take(),
        Some(json!(false)),
        "the import was still pending when dropped"
    );
    pool.run_until(host.settled());
    release.send(()).unwrap();
    pool.run_until(finished).unwrap();
    assert_eq!(host.kernel().diagnostics().calls, 0);
    // A session exists only if the provider ran before its cancellation; any session created
    // was destroyed (none stays live), at most once.
    let ran = pace.waits.get();
    assert!(ran <= 1);
    let checked = store.clone();
    let count = root(&mut pool, &host, async move |context| {
        checked
            .call_portable::<_, u64>(&context, "closed", json!([]))
            .await
    });
    match count {
        Ok(closed) => assert!(closed <= u64::from(ran)),
        Err(error) => {
            // Cancellation may race the provider's nested ordinary import. That import has
            // no WIT error lane, so the documented outcome is quarantine, not a reusable Store.
            assert_eq!(error.reason, "wasm_executor_quarantined", "{error}");
            assert!(error.message.contains("cancelled/call"), "{error}");
        }
    }
    assert_eq!(engine.resources(), 0);
    assert_eq!(host.kernel().diagnostics().references, claims);
    relay.release().unwrap();
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The guest abandons an async import whose provider call has demonstrably started.
///
/// The compiled importer starts `open-later`, then waits on `closed`, which the controlled
/// provider answers only once its `open-later` call is running. Only then does the guest drop
/// the pending import and continue. The provider's call alone is cancelled (its signal fires,
/// the root's does not), but it stays physically owned: while its gate is held the importer's
/// call does not settle and both calls stay counted. Once the provider answers, its owned result
/// has no receiver and is closed exactly once; then the importer's call settles with no call,
/// resource or claim left. A second round proves the importer and provider stay healthy.
fn an_abandoned_started_import_is_joined_and_closes_its_late_result(interpreted: bool) {
    let Some(consuming) = fixture("TEMBRA_WASM_RESOURCE_CONSUMER") else {
        return;
    };
    let engine = engine(interpreted);
    let store = Rc::new(LateStore::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        late_store(store.clone()),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let claims = host.kernel().diagnostics().references;
    for round in 1..=2_u32 {
        store.log.borrow_mut().clear();
        let open = store.hold_open_later();
        let answer = Rc::new(RefCell::new(None));
        let (called, answered, owner, observed) =
            (relay.clone(), answer.clone(), host.clone(), store.clone());
        let finished = pool
            .spawner()
            .spawn_local_with_handle(async move {
                owner
                    .root("alice", move |context| {
                        async move {
                            let result = called
                                .call::<_, Value>(&context, "abandon-started", json!([3]))
                                .await?;
                            observed.note("importer:answer");
                            *answered.borrow_mut() =
                                Some((result, context.signal().is_cancelled()));
                            Ok(())
                        }
                        .boxed_local()
                    })
                    .await
            })
            .unwrap();
        // The checkpoint answered only after the provider started, and the guest went on past
        // the drop to its second `closed` call.
        until(&mut pool, || store.answered.get() == 2 * round);
        assert_eq!(store.position("provider:start"), Some(0), "{:?}", store.log);
        // Logical cancellation of the provider's call.
        until(&mut pool, || {
            store
                .signal
                .borrow()
                .as_ref()
                .is_some_and(tembra::Cancellation::is_cancelled)
        });
        // Physical ownership: give the importer every chance to settle while the gate is held.
        for _ in 0..50 {
            pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            answer.borrow().is_none(),
            "settled before the provider ended"
        );
        assert_eq!(store.position("provider:end"), None);
        assert_eq!(store.closed.get(), round - 1);
        assert_eq!(
            host.kernel().diagnostics().calls,
            2,
            "the importer's and the provider's calls stay counted"
        );
        open.send(()).unwrap();
        pool.run_until(finished).unwrap();
        assert_eq!(
            answer.borrow_mut().take(),
            Some((json!(false), false)),
            "the import was pending when dropped, and the root was never cancelled"
        );
        assert!(
            store.position("provider:end") < store.position("importer:answer"),
            "{:?}",
            store.log
        );
        pool.run_until(host.settled());
        assert_eq!(
            store.closed.get(),
            round,
            "the late owned result is closed exactly once"
        );
        assert_eq!(host.kernel().diagnostics().calls, 0);
        assert_eq!(host.kernel().diagnostics().references, claims);
        assert_eq!(engine.resources(), 0);
    }
    // The importer still serves calls through the same provider; nothing was closed twice.
    let called = relay.clone();
    let closed = root(&mut pool, &host, async move |context| {
        called.call::<_, Value>(&context, "closed", json!([])).await
    })
    .unwrap();
    assert_eq!(closed, json!(2));
    relay.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(store.closed.get(), 2);
    assert_eq!(engine.resources(), 0);
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A typed key is data the instance keeps, not an object address: equal keys open distinct
/// sessions, each with its own key and total, and each destroyed once by its own drop.
fn equal_keys_open_independent_native_sessions(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let (opened, observed) = root(&mut pool, &host, async move |context| {
        let mut opened = Vec::new();
        for key in [FIRST_KEY, FIRST_KEY, SECOND_KEY] {
            opened.push(
                called
                    .call_portable::<_, Owned>(&context, OPEN_KEYED, (key, KEYED_SEED))
                    .await?,
            );
        }
        let mut observed = Vec::new();
        for (session, n) in opened.iter().zip(1_u64..) {
            let total = session.call_portable::<_, u64>(&context, ADD, (n,)).await?;
            let key = session
                .call_portable::<_, (String, String)>(&context, KEY, json!([]))
                .await?;
            observed.push((key, total));
        }
        Ok((opened, observed))
    })
    .unwrap();
    let owned = |(left, right): (&str, &str)| (left.to_owned(), right.to_owned());
    assert_eq!(
        observed,
        vec![
            (owned(FIRST_KEY), KEYED_SEED + 1),
            (owned(FIRST_KEY), KEYED_SEED + 2),
            (owned(SECOND_KEY), KEYED_SEED + 3),
        ]
    );
    assert_eq!(engine.resources(), 3);
    // Each drop destroys exactly its own instance; the others keep their state.
    let mut opened = opened.into_iter();
    let first = opened.next().unwrap();
    drop(first);
    assert_eq!(closed(&mut pool, &host, &store), 1);
    let rest: Vec<_> = opened.collect();
    let totals = root(&mut pool, &host, async move |context| {
        let mut totals = Vec::new();
        for session in &rest {
            totals.push(
                session
                    .call_portable::<_, u64>(&context, TOTAL, json!([]))
                    .await?,
            );
        }
        Ok((totals, rest))
    })
    .unwrap();
    assert_eq!(totals.0, vec![KEYED_SEED + 2, KEYED_SEED + 3]);
    drop(totals.1);
    assert_eq!(closed(&mut pool, &host, &store), 3);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A compiled importer opens keyed sessions through its plain WIT import (which still reaches
/// the provider's `main` object): keys round-trip through both component boundaries, equal keys
/// stay independent, and the three drops are destroyed only after the importer's call settles.
fn a_compiled_importer_opens_independent_keyed_sessions(interpreted: bool) {
    let (Some(provided), Some(consuming)) = (
        fixture("TEMBRA_WASM_RESOURCE_PROVIDER"),
        fixture("TEMBRA_WASM_RESOURCE_CONSUMER"),
    ) else {
        return;
    };
    let engine = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(Rc::default()),
        provider(&engine, &provided, "store"),
        consumer(&engine, &consuming, "relay", "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let relay = reference(&mut pool, &host, &format!("relay::{RELAY}"), "alice");
    let called = relay.clone();
    let report = root(&mut pool, &host, async move |context| {
        called
            .call::<_, Value>(
                &context,
                "keyed",
                json!([
                    [FIRST_KEY.0, FIRST_KEY.1],
                    [SECOND_KEY.0, SECOND_KEY.1],
                    KEYED_SEED
                ]),
            )
            .await
    })
    .unwrap();
    let session = |key: (&str, &str), total: u64| json!({"key": [key.0, key.1], "total": total});
    assert_eq!(
        report["sessions"],
        json!([
            session(FIRST_KEY, KEYED_SEED + 1),
            session(FIRST_KEY, KEYED_SEED + 2),
            session(SECOND_KEY, KEYED_SEED + 3),
        ]),
        "{report}"
    );
    // The importer had dropped all three, yet no destructor had run inside its call.
    assert_eq!(report["closedBeforeSettlement"], json!(0), "{report}");
    // All three were destroyed once the call settled, with no later importer call.
    assert_eq!(closed(&mut pool, &host, &store), 3);
    assert_eq!(engine.resources(), 0);
    relay.release().unwrap();
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// One compiled revision has one Store and serves one callback at a time: a call on session B,
/// from another root and principal, stays queued while session A's async method is parked, and
/// runs only after A has physically finished. Sessions are not parallel decoders.
fn one_revision_serves_its_sessions_one_call_at_a_time(interpreted: bool) {
    let Some(bytes) = fixture("TEMBRA_WASM_RESOURCE_PROVIDER") else {
        return;
    };
    let engine = engine(interpreted);
    let pace = Rc::new(Pace::default());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pacer(pace.clone()),
        provider(&engine, &bytes, "store"),
    ])
    .unwrap();
    let store = reference(&mut pool, &host, &sessions("store"), "alice");
    let called = store.clone();
    let (a, b) = root(&mut pool, &host, async move |context| {
        let a = called
            .call_portable::<_, Owned>(&context, OPEN_KEYED, (FIRST_KEY, 1_u64))
            .await?;
        let b = called
            .call_portable::<_, Owned>(&context, OPEN_KEYED, (SECOND_KEY, 2_u64))
            .await?;
        Ok((Rc::new(a), Rc::new(b)))
    })
    .unwrap();
    let (open, gate) = futures::channel::oneshot::channel();
    *pace.gate.borrow_mut() = Some(gate);
    // Completion order and results, recorded by each caller when its call returns.
    let finished = Rc::new(RefCell::new(Vec::<(&str, u64)>::new()));
    let spawner = pool.spawner();
    let call = |principal: &'static str, session: Rc<Owned>, method: &'static str, args: Value| {
        let (owner, finished) = (host.clone(), finished.clone());
        spawner
            .spawn_local(async move {
                let result = owner
                    .root(principal, move |context| {
                        async move {
                            session
                                .call_portable::<_, u64>(&context, method, args)
                                .await
                        }
                        .boxed_local()
                    })
                    .await
                    .unwrap();
                finished.borrow_mut().push((principal, result));
            })
            .unwrap();
    };
    call("alice", a.clone(), PACED, json!([1]));
    // A is parked inside the provider Store, on the closed gate.
    until(&mut pool, || pace.waits.get() == 1);
    call("bob", b.clone(), TOTAL, json!([]));
    // B is admitted by the kernel but waits for the revision's Store; give it every chance to
    // run, which would finish it at once if calls were parallel.
    for _ in 0..50 {
        pool.run_until_stalled();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(finished.borrow().is_empty(), "{:?}", finished.borrow());
    open.send(()).unwrap();
    until(&mut pool, || finished.borrow().len() == 2);
    // A finished (1 + 1 * 10) before B could run (2).
    assert_eq!(*finished.borrow(), vec![("alice", 11), ("bob", 2)]);
    pool.run_until(host.settled());
    assert_eq!(engine.resources(), 2);
    assert_eq!(closed(&mut pool, &host, &store), 0);
    drop(a);
    assert_eq!(closed(&mut pool, &host, &store), 1);
    drop(b);
    assert_eq!(closed(&mut pool, &host, &store), 2);
    assert_eq!(engine.resources(), 0);
    store.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
