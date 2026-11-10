//! Compiled components call selected Orbs through the same scoped references as source Orbs.
//!
//! Fixtures are real components encoded from small core modules and a WIT world, so they run
//! without prebuilt guest toolchains: a synchronous import, a P3 `async func` import, and an
//! async-lifted export that publishes its result before issuing its import (a task tail).
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc, time::Duration};
use tembra::{Host, Orb, Reference, Service};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

#[path = "support/components.rs"]
mod components;
use components::*;

/// One observed dependency invocation: caller principal and argument.
type Calls = Rc<RefCell<Vec<(String, u32)>>>;

/// A trusted Rust provider of `world`'s single export whose result arrives after a real delay.
fn native(world: &str, name: &str, method: &'static str, delay: Duration, calls: Calls) -> Orb {
    let service = Contract::from_source("deps.wit", WIT, Some(world))
        .unwrap()
        .plan(name, 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new(name, 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .method(method, move |_, context, (value,): (u32,)| {
                let calls = calls.clone();
                async move {
                    let principal = context.principal()?;
                    futures_timer::Delay::new(delay).await;
                    // Recorded after the delay: an entry proves physical completion.
                    calls.borrow_mut().push((principal, value));
                    Ok(value * 3)
                }
            }),
    )
}

/// Acquire one live reference for `principal`.
fn reference(pool: &mut LocalPool, host: &Host, service: &str, principal: &str) -> Rc<Reference> {
    let reference = Rc::new(host.get(service, vec!["main".into()], principal).unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Call one method under a fresh root of `principal`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    value: u32,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root(principal, move |context| {
        async move {
            reference
                .call::<_, Value>(&context, method, json!([value]))
                .await
        }
        .boxed_local()
    }))
}

/// A compiled consumer calls a compiled provider through its selected default-key reference.
#[test]
fn compiled_component_calls_a_compiled_provider() {
    let engine = Engine::new(Limits::default()).unwrap();
    let provider = engine
        .load(
            &artifact("math-provider", MATH_PROVIDER, "math"),
            Install::default(),
        )
        .unwrap();
    let consumer = engine
        .load(
            &artifact("consumer", CONSUMER, "consumer"),
            Install::default().requires(MATH, "math", 1),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![provider, consumer]).unwrap();
    let calc = reference(&mut pool, &host, &format!("consumer::{CALC}"), "alice");
    assert_eq!(engine.instances(), 2);
    assert_eq!(
        call(&mut pool, &host, &calc, "alice", "quadruple", 5).unwrap(),
        20
    );
    assert_eq!(
        call(&mut pool, &host, &calc, "bob", "quadruple", 7).unwrap(),
        28
    );
    calc.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// An unselected or mismatched dependency is refused before any Store starts.
#[test]
fn compiled_imports_require_a_selected_provider() {
    let engine = Engine::new(Limits::default()).unwrap();
    let error = engine
        .load(
            &artifact("consumer", CONSUMER, "consumer"),
            Install::default(),
        )
        .err()
        .expect("an unselected Orb import is refused");
    assert!(error.message.contains("provider"), "{error}");
    assert_eq!(engine.instances(), 0);
}

/// A synchronous WIT import blocks only the guest fiber while a trusted provider's delayed
/// result is awaited under the caller's own principal.
///
/// A synchronously lifted export cannot instead block on an `async func` import: the Component
/// Model traps "cannot block a synchronous task before returning". Async imports belong to
/// async-lifted exports, as in the task-tail fixture.
#[test]
fn synchronous_import_awaits_a_delayed_native_provider() {
    let engine = Engine::new(Limits::default()).unwrap();
    let calls = Calls::default();
    let math = native(
        "math-provider",
        "math",
        "double",
        Duration::from_millis(50),
        calls.clone(),
    );
    let consumer = engine
        .load(
            &artifact("consumer", CONSUMER, "consumer"),
            Install::default().requires(MATH, "math", 1),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![math, consumer]).unwrap();
    let calc = reference(&mut pool, &host, &format!("consumer::{CALC}"), "alice");
    // The trusted provider triples, so two dependency calls multiply by nine.
    assert_eq!(
        call(&mut pool, &host, &calc, "alice", "quadruple", 4).unwrap(),
        36
    );
    assert_eq!(
        *calls.borrow(),
        vec![("alice".to_owned(), 4), ("alice".to_owned(), 12)]
    );
    calc.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// A P3 export publishes its result first and imports afterwards. The late import still carries
/// the originating call's authority, and the call settles only after that work has finished.
#[test]
fn task_tail_imports_keep_their_originating_authority() {
    let engine = Engine::new(Limits::default()).unwrap();
    let calls = Calls::default();
    let slow = native(
        "slow-provider",
        "slow",
        "delayed",
        Duration::from_millis(50),
        calls.clone(),
    );
    let consumer = engine
        .load(
            &artifact("tail-consumer", TAIL_CONSUMER, "consumer"),
            Install::default().requires(SLOW, "slow", 1),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![slow, consumer]).unwrap();
    let tail = reference(&mut pool, &host, &format!("consumer::{TAIL}"), "alice");
    assert_eq!(call(&mut pool, &host, &tail, "alice", "run", 7).unwrap(), 7);
    // The returned value was published before the import; settlement waited for the import.
    assert_eq!(*calls.borrow(), vec![("alice".to_owned(), 7)]);
    assert_eq!(call(&mut pool, &host, &tail, "bob", "run", 9).unwrap(), 9);
    assert_eq!(
        *calls.borrow(),
        vec![("alice".to_owned(), 7), ("bob".to_owned(), 9)]
    );
    tail.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// An import issued before any invocation exists has no authority and fails instantiation.
#[test]
fn initialization_imports_are_refused() {
    let engine = Engine::new(Limits::default()).unwrap();
    let error = engine
        .load(
            &artifact("eager-consumer", EAGER_CONSUMER, "eager"),
            Install::default().requires(MATH, "math", 1),
        )
        .err()
        .expect("an import during instantiation is refused");
    assert!(error.message.contains("initialization failed"), "{error}");
    assert!(
        error.message.contains("no admitted invocation"),
        "the refusal must come from missing call authority: {error}"
    );
    assert_eq!(engine.instances(), 0);
}

/// A dependency that calls back into its suspended compiled caller is refused by the kernel's
/// execution frames instead of waiting on that caller's own admission. The refusal reaches the
/// synchronous import as a trap, which quarantines only the importer.
#[test]
fn causal_reentry_through_a_trusted_provider_is_refused() {
    let engine = Engine::new(Limits {
        timeout_ms: 5_000,
        ..Limits::default()
    })
    .unwrap();
    let consumer = engine
        .load(
            &artifact("consumer", CONSUMER, "consumer"),
            Install::default().requires(MATH, "math", 1),
        )
        .unwrap();
    let service = Contract::from_source("deps.wit", WIT, Some("math-provider"))
        .unwrap()
        .plan("math", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    let back = Rc::new(RefCell::new(None::<Rc<Reference>>));
    let bounce = back.clone();
    let math = Orb::new("math", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .method("double", move |_, context, (value,): (u32,)| {
                let calc = bounce.borrow().clone().expect("callback reference");
                async move {
                    calc.call::<_, Value>(&context, "quadruple", json!([value]))
                        .await
                        .map(|_| value * 2)
                }
            }),
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![math, consumer]).unwrap();
    let calc = reference(&mut pool, &host, &format!("consumer::{CALC}"), "alice");
    *back.borrow_mut() = Some(calc.clone());
    let started = std::time::Instant::now();
    let error = call(&mut pool, &host, &calc, "alice", "quadruple", 1).unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reentry waited instead of being refused"
    );
    assert!(error.message.contains("causal reentry"), "{error}");
    let quarantined = call(&mut pool, &host, &calc, "alice", "quadruple", 1).unwrap_err();
    assert!(
        quarantined.message.contains("component trap"),
        "{quarantined}"
    );
    back.borrow_mut().take();
    calc.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Cancelling the caller while a compiled import waits never abandons physical work: the
/// in-flight dependency call settles, then the cancelled invocation's next import is refused.
/// A synchronous WIT import can report that refusal only by trapping, so the importer's Store is
/// quarantined and retired; no error value is fabricated for the guest.
#[test]
fn cancellation_settles_inflight_work_then_refuses_later_imports() {
    let engine = Engine::new(Limits::default()).unwrap();
    let calls = Calls::default();
    let math = native(
        "math-provider",
        "math",
        "double",
        Duration::from_millis(100),
        calls.clone(),
    );
    let consumer = engine
        .load(
            &artifact("consumer", CONSUMER, "consumer"),
            Install::default().requires(MATH, "math", 1),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![math, consumer]).unwrap();
    let calc = reference(&mut pool, &host, &format!("consumer::{CALC}"), "alice");
    let called = calc.clone();
    let error = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let cancel = context.clone();
                let (result, ()) = futures::join!(
                    called.call::<_, Value>(&context, "quadruple", json!([2])),
                    async move {
                        futures_timer::Delay::new(Duration::from_millis(20)).await;
                        cancel.cancel().unwrap();
                    }
                );
                result
            }
            .boxed_local()
        }))
        .unwrap_err();
    assert!(
        error.message.contains("cancelled") || error.message.contains("component trap"),
        "{error}"
    );
    // The observer may return first; the host keeps driving the admitted physical call.
    pool.run_until(futures_timer::Delay::new(Duration::from_millis(300)));
    assert_eq!(
        *calls.borrow(),
        vec![("alice".to_owned(), 2)],
        "the in-flight import settled and no import ran after cancellation"
    );
    assert_eq!(engine.instances(), 0, "the quarantined Store was retired");
    let quarantined = call(&mut pool, &host, &calc, "alice", "quadruple", 1).unwrap_err();
    assert!(
        quarantined.message.contains("component trap"),
        "{quarantined}"
    );
    calc.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// Declarations and execution use distinct entrypoints; refusals leave the Store usable.
#[test]
fn compiled_commands_cannot_cross_execution_entrypoints() {
    use tembra_executor_wasm::PortEvent;

    let engine = Engine::new(Limits::default()).unwrap();
    let artifact =
        tembra_contracts::Artifact::decode(&artifact("math-provider", MATH_PROVIDER, "math"))
            .unwrap();
    let port = engine
        .open_port(
            &artifact,
            &Default::default(),
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
    let create = json!({"kind":"create", "environment":1, "service":null});
    let invoke =
        json!({"kind":"invoke", "environment":1, "service":"math", "method":"double", "args":[3]});
    let release = json!({"kind":"release", "environment":1});
    let (ticket, events) = port.begin(create.to_string()).unwrap();
    let PortEvent::Complete(result) = events.receive().unwrap() else {
        panic!("execution entrypoint issued a declaration request")
    };
    assert!(result.unwrap_err().message.contains("entrypoint"));
    ticket.finish().unwrap();
    for packet in [invoke.clone(), release] {
        let error = port
            .declare(packet, |_, _| {
                panic!("invalid declaration issued a host request")
            })
            .unwrap_err();
        assert!(error.message.contains("entrypoint"), "{error}");
    }
    port.declare(create, |_, _| panic!("provider declares no imports"))
        .unwrap();
    let (ticket, events) = port.begin(invoke.to_string()).unwrap();
    let PortEvent::Complete(result) = events.receive().unwrap() else {
        panic!("provider calls no imports")
    };
    assert_eq!(
        serde_json::from_str::<Value>(&result.unwrap()).unwrap(),
        json!(6)
    );
    ticket.finish().unwrap();
}
