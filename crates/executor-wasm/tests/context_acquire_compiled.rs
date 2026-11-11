//! Compiled WIT views select kernel instances, rather than fresh provider-owned resources.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use tembra::{AcquisitionOptions, Host, Orb, Reference, Service};
use tembra_contracts::{ComponentRoles, Contract};
use tembra_executor_wasm::{Engine, Install, Limits};

const COUNTER: &str = "example:keyed/counter@1.0.0";
const VIEW: &str = "example:keyed/counter-ref@1.0.0";
const ROUTER: &str = "router::example:keyed/compiled-router@1.0.0";
const WIT: &str = include_str!("wit/keyed/keyed.wit");

fn artifact() -> Vec<u8> {
    let bytes = std::fs::read(
        std::env::var_os("TEMBRA_WASM_KEYED_ROUTER").expect("TEMBRA_WASM_KEYED_ROUTER"),
    )
    .unwrap();
    Contract::from_source("keyed.wit", WIT, Some("router"))
        .unwrap()
        .component_artifact(
            &bytes,
            "router",
            1,
            &ComponentRoles {
                references: [(VIEW.into(), COUNTER.into())].into(),
                ..Default::default()
            },
        )
        .unwrap()
}

struct Counter {
    label: String,
    value: Cell<u32>,
}

fn provider(activated: Rc<RefCell<Vec<String>>>) -> Orb {
    let wit = format!("{WIT}\nworld provider {{ export counter; }}");
    let declaration = Contract::from_source("keyed.wit", &wit, Some("provider"))
        .unwrap()
        .plan("keyed", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("keyed", 1).service(
        Service::new(declaration.name, move |declaration| {
            let address = declaration.address()?;
            let key = address.key.join("/");
            if key == "denied" || key == "main" {
                return Err(tembra::Error::new(
                    tembra::core::Code::Forbidden,
                    "key",
                    "unapproved key",
                ));
            }
            if matches!(key.as_str(), "slow" | "settles" | "barrier") {
                let delay = match key.as_str() {
                    "settles" => 50,
                    "barrier" => 150,
                    _ => 250,
                };
                declaration.resource_async(
                    "connection",
                    1,
                    &[],
                    move |signal| {
                        async move {
                            let wait = std::pin::pin!(futures_timer::Delay::new(
                                Duration::from_millis(delay)
                            ));
                            let cancelled = std::pin::pin!(signal.cancelled());
                            let _ = futures::future::select(wait, cancelled).await;
                            signal.check()?;
                            Ok(())
                        }
                        .boxed_local()
                    },
                    |_, _| async { Ok(()) }.boxed_local(),
                )?;
            }
            let label = format!("{key}@{}", address.principal);
            activated.borrow_mut().push(label.clone());
            Ok(Counter {
                label,
                value: Cell::new(0),
            })
        })
        .signature(declaration.signature)
        .method("add", |counter, _, (amount,): (u32,)| async move {
            // A synchronous WIT method still awaits real host work.
            futures_timer::Delay::new(Duration::from_millis(1)).await;
            let value = counter.value.get() + amount;
            counter.value.set(value);
            Ok(value)
        })
        .method("who", |counter, _, _: [Value; 0]| async move {
            Ok(counter.label.clone())
        }),
    )
}

fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    args: Value,
) -> tembra::Result<String> {
    let reference = reference.clone();
    pool.run_until(host.root(principal, move |context| {
        async move { reference.call_portable(&context, method, args).await }.boxed_local()
    }))
}

fn scenario(interpreted: bool) {
    let limits = Limits {
        timeout_ms: 10_000,
        packet_bytes: 8192,
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap();
    let router = engine
        .load(&artifact(), Install::default().requires(VIEW, "keyed", 1))
        .unwrap();
    let activated = Rc::new(RefCell::new(Vec::new()));
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![provider(activated.clone()), router])
        .unwrap();
    let reference = Rc::new(host.get(ROUTER, vec!["main".into()], "host").unwrap());
    pool.run_until(reference.ready()).unwrap();
    assert!(
        activated.borrow().is_empty(),
        "a view must not activate main"
    );

    for principal in ["alice", "bob"] {
        assert_eq!(
            call(
                &mut pool,
                &host,
                &reference,
                principal,
                "route",
                json!(["a", "b", 2])
            )
            .unwrap(),
            format!("2,4,2 a@{principal} b@{principal}")
        );
        assert_eq!(host.kernel().diagnostics().references, 1);
        assert_eq!(
            engine.resources(),
            0,
            "local references are not provider resources"
        );
    }
    let labels = activated.borrow();
    for label in ["a@alice", "b@alice", "a@bob", "b@bob"] {
        assert_eq!(
            labels.iter().filter(|seen| *seen == label).count(),
            1,
            "{label}"
        );
    }
    drop(labels);
    // Unapproved request data takes the WIT error lane and leaves the revision usable.
    let refused = call(
        &mut pool,
        &host,
        &reference,
        "alice",
        "probe",
        json!(["denied"]),
    )
    .unwrap();
    assert!(
        refused.contains("refused:") && refused.contains("unapproved key"),
        "{refused}"
    );
    assert_eq!(
        call(
            &mut pool,
            &host,
            &reference,
            "alice",
            "probe",
            json!(["ok"])
        )
        .unwrap(),
        "ok@alice"
    );
    assert_eq!(host.kernel().diagnostics().references, 1);

    let oversized = call(
        &mut pool,
        &host,
        &reference,
        "alice",
        "probe",
        json!(["oversized"]),
    )
    .unwrap();
    assert!(
        oversized.contains("refused:") && oversized.contains("packet limit"),
        "{oversized}"
    );
    assert_eq!(
        call(
            &mut pool,
            &host,
            &reference,
            "alice",
            "probe",
            json!(["after-limit"])
        )
        .unwrap(),
        "after-limit@alice"
    );

    // Observe drops before ending the root: root teardown alone must not hide stranded claims.
    let called = reference.clone();
    let observer = host.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let _: String = called
                .call_portable(&context, "route", json!(["drop-a", "drop-b", 1]))
                .await?;
            assert_eq!(
                observer.kernel().diagnostics().references,
                1,
                "guest drops left claims in the live root"
            );
            let _: String = called
                .call_portable(&context, "probe", json!(["abandon"]))
                .await?;
            assert_eq!(
                observer.kernel().diagnostics().references,
                1,
                "abandoned get left a claim"
            );
            let waited: String = called
                .call_portable(&context, "probe", json!(["abandon-wait"]))
                .await?;
            assert_eq!(waited, "barrier@alice");
            assert_eq!(
                observer.kernel().diagnostics().references,
                1,
                "an abandoned reply published a live claim"
            );
            let kept: String = called
                .call_portable(&context, "probe", json!(["keep"]))
                .await?;
            assert_eq!(kept, "keep@alice");
            assert_eq!(observer.kernel().diagnostics().references, 2);
            let same_root: String = called
                .call_portable(&context, "probe", json!(["retained"]))
                .await?;
            assert_eq!(same_root, "keep@alice");
            assert_eq!(observer.kernel().diagnostics().references, 2);
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert_eq!(
        host.kernel().diagnostics().references,
        1,
        "root end did not expire its retained reference"
    );

    let observer = host.clone();
    let called = reference.clone();
    let (outcome, live) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let acquire = called.call_portable::<_, String>(&context, "probe", json!(["slow"]));
                let cancel = async {
                    futures_timer::Delay::new(Duration::from_millis(20)).await;
                    context.cancel()
                };
                let (outcome, cancelled) = futures::join!(acquire, cancel);
                cancelled?;
                Ok((outcome, observer.kernel().diagnostics().references))
            }
            .boxed_local()
        }))
        .unwrap();
    assert!(!matches!(outcome, Ok(ref result) if result == "slow@alice"));
    assert_eq!(live, 1, "cancellation left an acquired claim");
    assert_eq!(
        call(
            &mut pool,
            &host,
            &reference,
            "alice",
            "probe",
            json!(["after-cancel"])
        )
        .unwrap(),
        "after-cancel@alice"
    );

    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert_eq!(host.kernel().diagnostics().references, 0);
}

#[test]
#[ignore = "requires TEMBRA_WASM_KEYED_ROUTER"]
fn compiled_references_select_keys_and_keep_root_authority() {
    scenario(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_KEYED_ROUTER"]
fn interpreted_compiled_references_select_keys_and_keep_root_authority() {
    scenario(true);
}

fn retained(interpreted: bool, live: bool) {
    let engine = if interpreted {
        Engine::interpreted(Limits::default())
    } else {
        Engine::new(Limits::default())
    }
    .unwrap();
    let router = engine
        .load(&artifact(), Install::default().requires(VIEW, "keyed", 1))
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let activated = Rc::new(RefCell::new(Vec::new()));
    host.install(vec![provider(activated.clone()), router])
        .unwrap();
    let first = Rc::new(host.get(ROUTER, vec!["first".into()], "host").unwrap());
    let second = Rc::new(host.get(ROUTER, vec!["second".into()], "host").unwrap());
    pool.run_until(first.ready()).unwrap();
    pool.run_until(second.ready()).unwrap();
    let active = if live {
        let (started, ready) = oneshot::channel();
        let (release, hold) = oneshot::channel();
        let owner = host.clone();
        let called = first.clone();
        let finished = pool
            .spawner()
            .spawn_local_with_handle(async move {
                owner
                    .root("alice", move |context| {
                        async move {
                            let kept: String = called
                                .call_portable(&context, "probe", json!(["keep"]))
                                .await?;
                            assert_eq!(kept, "keep@alice");
                            let _ = started.send(());
                            let _ = hold.await;
                            // Retiring a compiled Store releases its views, not the unrelated
                            // root that originally acquired them. That root still authorizes
                            // native calls after the other root's forbidden use traps the Store.
                            context.check()?;
                            let native = context.get(
                                &format!("keyed::{COUNTER}"),
                                vec!["surviving-root".into()],
                                AcquisitionOptions {
                                    api_version: 1,
                                    methods: ["who".into()].into(),
                                },
                            )?;
                            native.ready().await?;
                            let label: String =
                                native.call_portable(&context, "who", json!([])).await?;
                            assert_eq!(label, "surviving-root@alice");
                            context.check()?;
                            Ok(())
                        }
                        .boxed_local()
                    })
                    .await
            })
            .unwrap();
        pool.run_until(ready).unwrap();
        Some((release, finished))
    } else {
        assert_eq!(
            call(&mut pool, &host, &first, "alice", "probe", json!(["keep"])).unwrap(),
            "keep@alice"
        );
        None
    };
    assert_eq!(engine.instances(), 1, "the compiled Store is still live");
    assert_eq!(
        host.kernel().diagnostics().references,
        if live { 3 } else { 2 },
        "the first root owns its view until it ends or its Store retires"
    );
    // Both environments share the compiled heap. A retained slot cannot select a new claim
    // allocated under a later root, even if that root has the same principal.
    let error = call(
        &mut pool,
        &host,
        &second,
        "alice",
        "probe",
        json!(["retained"]),
    )
    .unwrap_err();
    assert!(
        activated
            .borrow()
            .iter()
            .any(|label| label == "fresh@alice"),
        "the forbidden attempt first acquired a separate temporary claim"
    );
    assert_eq!(
        engine.instances(),
        0,
        "the forbidden import traps and physically retires the compiled Store"
    );
    if let Some((release, finished)) = active {
        assert!(error.message.contains("belongs to another root"), "{error}");
        assert_eq!(
            host.kernel().diagnostics().references,
            2,
            "Store retirement releases both its retained view and the temporary claim"
        );
        release.send(()).unwrap();
        pool.run_until(finished).unwrap();
    }
    assert_eq!(host.kernel().diagnostics().references, 2);
    first.release().unwrap();
    second.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert_eq!(host.kernel().diagnostics().references, 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

#[test]
#[ignore = "requires TEMBRA_WASM_KEYED_ROUTER"]
fn retained_compiled_reference_cannot_cross_roots_or_alias_new_slots() {
    retained(false, false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_KEYED_ROUTER"]
fn interpreted_retained_compiled_reference_cannot_cross_roots_or_alias_new_slots() {
    retained(true, false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_KEYED_ROUTER"]
fn compiled_reference_cannot_cross_simultaneously_live_roots() {
    retained(false, true);
}

#[test]
#[ignore = "requires TEMBRA_WASM_KEYED_ROUTER"]
fn interpreted_compiled_reference_cannot_cross_simultaneously_live_roots() {
    retained(true, true);
}
