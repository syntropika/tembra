//! Native declarations use the same signed hook plans as compiled declarations.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use tembra::{Hook, Host, Orb, contracts::Contract, core::Requirement};
use tembra_orb_native_fixture::checkpoint;

const TRACE: tembra::core::ContextKey<String> = tembra::core::ContextKey::new("test/hook-trace", 1);

#[test]
fn a_generated_hook_composes_without_serde_or_clone_on_author_types() {
    const HOOK: &str = "example:checkpoint/inspect@1.0.0";
    let contract = Contract::from_source("checkpoint.wit", checkpoint::WIT, None).unwrap();
    let signature = contract.function_signature(HOOK, "apply", true).unwrap();
    let hook = Hook::<Value, Value>::waterfall(HOOK).signature(signature);
    let contributor = Orb::new("watch", 1)
        .requires(Requirement {
            orb: "calculator".into(),
            api_version: 2,
            hooks: [HOOK.into()].into(),
            methods: Default::default(),
            signatures: Default::default(),
            hook_signatures: Default::default(),
        })
        .contribute("calculator", &hook, 0, |context, input: Value| async move {
            assert_eq!(context.principal().unwrap(), "alice");
            assert_eq!(context.value(&TRACE).unwrap().as_deref(), Some("trace-7"));
            Ok(json!({"value": input["value"].as_i64().unwrap() + 1, "stopped": true}))
        });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![checkpoint::orb().unwrap(), contributor])
        .unwrap();
    let inspector = std::rc::Rc::new(
        host.get(
            "calculator::example:checkpoint/inspector@1.0.0",
            vec!["a".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(inspector.ready()).unwrap();
    let called = inspector.clone();
    let answer: Value = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let context = context.with(&TRACE, "trace-7".into())?;
                called.call_portable(&context, "inspect", (7,)).await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(answer, json!({"value": 8, "stopped": true}));
    inspector.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn a_hook_transport_failure_ends_its_generated_caller_without_imports() {
    let hook = signed_hook();
    let contributor = watch().contribute("calculator", &hook, 0, |_, _: Value| async {
        Err(tembra::Error::new(
            tembra::Code::Forbidden,
            "inspection",
            "denied",
        ))
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![checkpoint::orb().unwrap(), contributor])
        .unwrap();
    let inspector = inspector(&mut pool, &host);
    let called = inspector.clone();
    let answer = pool
        .run_until(host.root("alice", move |context| {
            async move {
                called
                    .call_portable::<_, Value>(&context, "inspect", (7,))
                    .await
            }
            .boxed_local()
        }))
        .unwrap_err();
    assert_eq!(
        (answer.code, answer.reason.as_str()),
        (tembra::Code::Forbidden, "inspection")
    );
    assert_eq!(host.kernel().diagnostics().calls, 0);
    inspector.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn cancelled_hooks_retain_the_declaring_object_until_the_participant_settles() {
    use futures::channel::oneshot;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    let entered = Rc::new(Cell::new(false));
    let seen = entered.clone();
    let cancelled = Rc::new(Cell::new(false));
    let stopped = cancelled.clone();
    let (open, gate) = oneshot::channel::<()>();
    let gate = RefCell::new(Some(gate));
    let hook = signed_hook();
    let contributor = watch().contribute("calculator", &hook, 0, move |context, input| {
        let gate = gate.borrow_mut().take().unwrap();
        let (seen, stopped) = (seen.clone(), stopped.clone());
        async move {
            seen.set(true);
            context.signal().cancelled().await;
            stopped.set(true);
            gate.await.unwrap();
            Ok(input)
        }
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![checkpoint::orb().unwrap(), contributor])
        .unwrap();
    let inspector = inspector(&mut pool, &host);
    let root = Rc::new(RefCell::new(None));
    let saved = root.clone();
    let called = inspector.clone();
    let answer = host
        .spawn_root("alice", move |context| {
            async move {
                saved.replace(Some(context.clone()));
                called
                    .call_portable::<_, Value>(&context, "inspect", (7,))
                    .await
            }
            .boxed_local()
        })
        .unwrap();
    pool.run_until_stalled();
    assert!(entered.get());
    inspector.release().unwrap();
    root.borrow().as_ref().unwrap().cancel().unwrap();
    pool.run_until_stalled();
    assert!(cancelled.get());
    assert!(host.kernel().diagnostics().calls > 0);
    assert!(host.kernel().diagnostics().objects > 0);
    open.send(()).unwrap();
    let _ = pool.run_until(answer).unwrap();
    pool.run_until(host.settled());
    assert_eq!(host.kernel().diagnostics().calls, 0);
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().objects, 0);
}

#[test]
fn a_nominal_signature_mismatch_is_refused_before_the_participant_runs() {
    use std::{cell::Cell, rc::Rc};
    let entered = Rc::new(Cell::new(false));
    let seen = entered.clone();
    let wrong =
        Hook::<Value, Value>::waterfall("example:checkpoint/inspect@1.0.0").signature("different");
    let contributor = watch().contribute("calculator", &wrong, 0, move |_, input| {
        seen.set(true);
        async move { Ok(input) }
    });
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let error = host
        .install(vec![checkpoint::orb().unwrap(), contributor])
        .unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (tembra::Code::Invalid, "rust_type")
    );
    assert!(!entered.get());
}

/// Prepare the same nominal hook contract as the generated declarer.
fn signed_hook() -> Hook<Value, Value> {
    const HOOK: &str = "example:checkpoint/inspect@1.0.0";
    let contract = Contract::from_source("checkpoint.wit", checkpoint::WIT, None).unwrap();
    Hook::waterfall(HOOK).signature(contract.function_signature(HOOK, "apply", true).unwrap())
}

/// Declare the contribution's existing hook dependency.
fn watch() -> Orb {
    Orb::new("watch", 1).requires(Requirement {
        orb: "calculator".into(),
        api_version: 2,
        hooks: ["example:checkpoint/inspect@1.0.0".into()].into(),
        methods: Default::default(),
        signatures: Default::default(),
        hook_signatures: Default::default(),
    })
}

/// Acquire an object distinct from the Orb's root.
fn inspector(pool: &mut LocalPool, host: &Host) -> std::rc::Rc<tembra::Reference> {
    let reference = std::rc::Rc::new(
        host.get(
            "calculator::example:checkpoint/inspector@1.0.0",
            vec!["a".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    reference
}

#[test]
fn generated_hook_handles_follow_reload_and_refuse_host_closure() {
    let hook = signed_hook();
    let contributor = watch().contribute("calculator", &hook, 0, |_, input: Value| async move {
        Ok(json!({"value": input["value"].as_i64().unwrap() + 1, "stopped": true}))
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![checkpoint::orb().unwrap(), contributor])
        .unwrap();
    let inspector = inspector(&mut pool, &host);
    for reload in [false, true] {
        if reload {
            pool.run_until(host.reload(vec![checkpoint::orb().unwrap()], 5_000))
                .unwrap();
        }
        let called = inspector.clone();
        let answer: Value = pool
            .run_until(host.root("alice", move |context| {
                async move { called.call_portable(&context, "inspect", (7,)).await }.boxed_local()
            }))
            .unwrap();
        assert_eq!(answer, json!({"value":8,"stopped":true}));
    }
    pool.run_until(host.close()).unwrap();
    let called = inspector.clone();
    let error = pool
        .run_until(host.root("alice", move |context| {
            async move {
                called
                    .call_portable::<_, Value>(&context, "inspect", (7,))
                    .await
            }
            .boxed_local()
        }))
        .unwrap_err();
    assert_eq!(error.code, tembra::Code::Closed);
    assert_eq!(host.kernel().diagnostics().calls, 0);
}
