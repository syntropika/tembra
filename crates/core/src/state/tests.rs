use std::{
    cell::{Cell as Counter, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{RawWaker, RawWakerVTable, Waker},
};

use futures::FutureExt;
use serde_json::{Value, json};

use crate::{Context, ErrorCode, ManualDriver, Result, RuntimeError};

use super::*;

#[test]
fn structured_state_normalization_preserves_previous_values_limits_and_errors() {
    fn previous(value: &Value) -> Result<Rc<Value>> {
        let encoded = tembra_contracts::canonical_json(value)
            .map_err(|error| RuntimeError::invalid(error.reason, error.message))?;
        tembra_contracts::parse_json(&encoded)
            .map(Rc::new)
            .map_err(|error| RuntimeError::invalid(error.reason, error.message))
    }
    fn compare(value: &Value) {
        let fingerprint = |error: RuntimeError| (error.code, error.reason, error.message);
        let before = value.clone();
        assert_eq!(
            normalize(value).map_err(fingerprint),
            previous(value).map_err(fingerprint),
            "input: {value:?}"
        );
        assert_eq!(*value, before);
    }
    for value in [
        json!(null),
        json!(true),
        json!(0),
        json!(-0.0),
        json!(17.0),
        json!(-17.0),
        json!(1.25),
        json!(0.1),
        json!(f64::from_bits(1)),
        json!(9_007_199_254_740_991_u64),
        json!(9_007_199_254_740_992_u64),
        json!(u64::MAX),
        json!(1.0e100),
        json!({"é":[1.0,-0.0,1.25,null,{"text":"quote\" slash\\ newline\n \u{0} 🦀"}]}),
        json!({"$serde_json::private::Number":"1.25"}),
    ] {
        compare(&value);
    }
    for text in [
        "1.00000000000000001",
        "9007199254740991.01",
        "0.100000000000000000000001",
        "1e-400",
        "-1e-400",
        "4.9406564584124654e-324",
    ] {
        compare(&Value::Number(text.parse().unwrap()));
    }
    for length in [1_048_573, 1_048_574, 1_048_575] {
        compare(&Value::String("a".repeat(length)));
    }
    compare(&Value::String("\u{0}".repeat(174_763)));
    for depth in [127, 128, 129] {
        let mut value = json!(null);
        for _ in 0..depth {
            value = json!([value]);
        }
        compare(&value);
    }
    let mut bits = 0x9e3779b97f4a7c15_u64;
    for _ in 0..4096 {
        bits ^= bits << 13;
        bits ^= bits >> 7;
        bits ^= bits << 17;
        if let Some(number) = serde_json::Number::from_f64(f64::from_bits(bits)) {
            compare(&Value::Number(number));
        }
    }
}

fn graph() -> (Rc<StateGraph>, Rc<RefCell<Vec<RuntimeError>>>) {
    let errors = Rc::new(RefCell::new(Vec::new()));
    let reported = errors.clone();
    let graph = StateGraph::new(
        Rc::new(|| Ok(())),
        Rc::new(|| Ok(())),
        Rc::new(|ctx| ctx.check(1)),
        Rc::new(move |error| reported.borrow_mut().push(error)),
        PublicationQueue::new(),
    );
    (graph, errors)
}

fn cell(graph: &Rc<StateGraph>, name: &str, value: Value) -> Cell {
    graph
        .declare_cell(
            name.into(),
            StateDeclaration::new(json!({"type": "integer"}), value),
        )
        .unwrap()
}

#[test]
fn failed_drafts_derivations_exposures_and_panics_leave_authority_intact() {
    let (graph, _) = graph();
    let source = cell(&graph, "source", json!(1));
    let derived = graph
        .derive(
            vec![source.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| {
                let value = values[0].value().as_i64().unwrap();
                if value == 3 {
                    return Err(RuntimeError::invalid("projection", "refused three"));
                }
                Ok(json!(value * 2))
            }),
        )
        .unwrap();
    let exposure = graph
        .expose(&derived, json!({"type": "integer", "maximum": 8}))
        .unwrap();
    graph.activate().unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    let original = source.snapshot().unwrap();
    let original_derived = derived.snapshot().unwrap();
    for value in [json!("wrong"), json!(3), json!(5)] {
        assert!(
            source
                .replace(&ctx, value, ReplaceOptions::default())
                .is_err()
        );
        assert_eq!(source.snapshot().unwrap(), original);
        assert_eq!(derived.snapshot().unwrap(), original_derived);
    }
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = source.change(&ctx, |_| -> Result<()> { panic!("native callback panic") });
        }))
        .is_err()
    );
    assert_eq!(source.snapshot().unwrap(), original);
    source
        .replace(&ctx, json!(2), ReplaceOptions::default())
        .unwrap();
    assert_eq!(derived.snapshot().unwrap().value(), &json!(4));
    drop(exposure);
}

#[test]
fn staged_dependency_set_and_topology_invalidate_competing_preparations() {
    let (graph, _) = graph();
    let a = cell(&graph, "a", json!(1));
    let b = cell(&graph, "b", json!(10));
    let sum = graph
        .derive(
            vec![a.readable(), b.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| {
                Ok(json!(
                    values[0].value().as_i64().unwrap() + values[1].value().as_i64().unwrap()
                ))
            }),
        )
        .unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    let prepared = a
        .prepare_replace(&ctx, json!(2), ReplaceOptions::default())
        .unwrap();
    b.replace(&ctx, json!(20), ReplaceOptions::default())
        .unwrap();
    assert_eq!(
        a.adopt(&ctx, &prepared).unwrap_err().reason,
        "stale_prepared"
    );
    assert_eq!(sum.snapshot().unwrap().value(), &json!(21));
    let prepared = a
        .prepare_replace(&ctx, json!(2), ReplaceOptions::default())
        .unwrap();
    let _exposure = graph
        .expose(&a.readable(), json!({"type": "integer", "maximum": 1}))
        .unwrap();
    assert_eq!(
        a.adopt(&ctx, &prepared).unwrap_err().reason,
        "stale_prepared"
    );
    assert_eq!(a.snapshot().unwrap().value(), &json!(1));
}

#[test]
fn native_derived_callbacks_cannot_read_old_authority_behind_staged_inputs() {
    let (graph, _) = graph();
    let source = cell(&graph, "source", json!(1));
    let captured = source.clone();
    let result = graph.derive(
        vec![source.readable()],
        Value::Bool(true),
        Rc::new(move |_| Ok(captured.snapshot()?.value().clone())),
    );
    assert_eq!(result.err().unwrap().reason, "state_reentrancy");
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    source
        .change(&ctx, |value| {
            *value = json!(2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn initializers_make_storage_readable_without_publishing_until_activation() {
    let (graph, _) = graph();
    let pending = graph
        .declare_cell(
            "async".into(),
            StateDeclaration::with_initializer(json!({"type": "integer"}), |_| {
                async { Ok(json!(4)) }.boxed_local()
            }),
        )
        .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let _subscription = pending
        .readable()
        .subscribe_events(move |event| {
            observed.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    assert_eq!(pending.snapshot().unwrap_err().code, ErrorCode::NotReady);
    graph.initialize(vec![(pending.clone(), json!(4))]).unwrap();
    assert_eq!(pending.snapshot().unwrap().value(), &json!(4));
    assert!(events.borrow().is_empty());
    graph.activate().unwrap();
    assert!(matches!(
        events.borrow().as_slice(),
        [StateEvent::Snapshot(_)]
    ));
}

#[test]
fn pending_dependency_initialization_invalidates_a_prepared_activation_write() {
    let (graph, _) = graph();
    let immediate = cell(&graph, "immediate", json!(1));
    let pending = graph
        .declare_cell(
            "pending".into(),
            StateDeclaration::with_initializer(json!({"type": "integer"}), |_| {
                futures::future::pending().boxed_local()
            }),
        )
        .unwrap();
    let sum = graph
        .derive(
            vec![pending.readable(), immediate.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| {
                Ok(json!(
                    values[0].value().as_i64().unwrap() + values[1].value().as_i64().unwrap()
                ))
            }),
        )
        .unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    immediate
        .replace(&ctx, json!(3), ReplaceOptions::default())
        .unwrap();
    assert_eq!(sum.snapshot().unwrap_err().code, ErrorCode::NotReady);
    let prepared = immediate
        .prepare_replace(&ctx, json!(4), ReplaceOptions::default())
        .unwrap();
    graph.initialize(vec![(pending, json!(10))]).unwrap();
    assert_eq!(sum.snapshot().unwrap().value(), &json!(13));
    assert_eq!(
        immediate.adopt(&ctx, &prepared).unwrap_err().reason,
        "stale_prepared"
    );
    assert_eq!(immediate.snapshot().unwrap().value(), &json!(3));
    assert_eq!(sum.snapshot().unwrap().value(), &json!(13));
}

#[test]
fn changed_silent_cell_and_derived_values_reach_already_initialized_listeners() {
    let (graph, _) = graph();
    let pending = graph
        .declare_cell(
            "pending".into(),
            StateDeclaration::with_initializer(json!({"type": "integer"}), |_| {
                futures::future::pending().boxed_local()
            }),
        )
        .unwrap();
    let doubled = graph
        .derive(
            vec![pending.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| Ok(json!(values[0].value().as_i64().unwrap() * 2))),
        )
        .unwrap();
    graph.initialize(vec![(pending.clone(), json!(1))]).unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let _cell_subscription = pending
        .readable()
        .subscribe_events(move |event| {
            observed.borrow_mut().push(("cell", event));
            Ok(())
        })
        .unwrap();
    let observed = events.clone();
    let _derived_subscription = doubled
        .subscribe_events(move |event| {
            observed.borrow_mut().push(("derived", event));
            Ok(())
        })
        .unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    pending
        .replace(&ctx, json!(2), ReplaceOptions::default())
        .unwrap();
    graph.activate().unwrap();
    let events = events.borrow();
    assert_eq!(events.len(), 4);
    assert_eq!(events[2].0, "cell");
    assert_eq!(events[2].1.snapshot().value(), &json!(2));
    assert!(matches!(events[2].1, StateEvent::Ops { .. }));
    assert_eq!(events[3].0, "derived");
    assert_eq!(events[3].1.snapshot().value(), &json!(4));
    assert!(matches!(events[3].1, StateEvent::Ops { .. }));
}

#[test]
fn external_updates_during_preparation_refresh_after_success_error_and_unwind() {
    let (external, _) = graph();
    let source = cell(&external, "source", json!(1));
    let (local, errors) = graph();
    let own = cell(&local, "own", json!(10));
    let projected = local
        .derive(
            vec![source.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| Ok(json!(values[0].value().as_i64().unwrap() * 2))),
        )
        .unwrap();
    ManualDriver::new()
        .run_until(local.hydrate_external())
        .unwrap();
    local.activate().unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    own.change(&ctx, |value| {
        source.replace(&ctx, json!(2), ReplaceOptions::default())?;
        source.replace(&ctx, json!(3), ReplaceOptions::default())?;
        *value = json!(11);
        Ok(())
    })
    .unwrap();
    assert_eq!(projected.snapshot().unwrap().value(), &json!(6));
    assert_eq!(own.snapshot().unwrap().value(), &json!(11));
    let error = own
        .change(&ctx, |_| {
            source.replace(&ctx, json!(4), ReplaceOptions::default())?;
            Err(RuntimeError::invalid("draft", "local draft refused"))
        })
        .unwrap_err();
    assert_eq!(error.reason, "draft");
    assert_eq!(projected.snapshot().unwrap().value(), &json!(8));
    assert_eq!(own.snapshot().unwrap().value(), &json!(11));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = own.change(&ctx, |_| -> Result<()> {
                source.replace(&ctx, json!(5), ReplaceOptions::default())?;
                panic!("native callback panic after independent write");
            });
        }))
        .is_err()
    );
    assert_eq!(source.snapshot().unwrap().value(), &json!(5));
    assert_eq!(projected.snapshot().unwrap().value(), &json!(10));
    assert_eq!(own.snapshot().unwrap().value(), &json!(11));
    assert!(errors.borrow().is_empty());
}

#[test]
fn deferred_import_refresh_invalidates_preparation_that_read_its_old_value() {
    let (external, _) = graph();
    let source = cell(&external, "source", json!(1));
    let (local, errors) = graph();
    let own = cell(&local, "own", json!(10));
    let sum = local
        .derive(
            vec![own.readable(), source.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| {
                Ok(json!(
                    values[0].value().as_i64().unwrap() + values[1].value().as_i64().unwrap()
                ))
            }),
        )
        .unwrap();
    ManualDriver::new()
        .run_until(local.hydrate_external())
        .unwrap();
    local.activate().unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    let prepared = own
        .prepare_change(&ctx, |value| {
            source.replace(&ctx, json!(20), ReplaceOptions::default())?;
            *value = json!(11);
            Ok(())
        })
        .unwrap();
    assert_eq!(sum.snapshot().unwrap().value(), &json!(30));
    assert_eq!(
        own.adopt(&ctx, &prepared).unwrap_err().reason,
        "stale_prepared"
    );
    assert_eq!(own.snapshot().unwrap().value(), &json!(10));
    assert!(errors.borrow().is_empty());
}

#[test]
fn external_projection_failure_reports_without_rolling_back_its_source() {
    let (external, _) = graph();
    let source = cell(&external, "source", json!(1));
    let (local, errors) = graph();
    let projected = local
        .derive(
            vec![source.readable()],
            json!({"type": "integer"}),
            Rc::new(|values| {
                let value = values[0].value().as_i64().unwrap();
                if value == 2 {
                    return Err(RuntimeError::invalid("projection", "two refused"));
                }
                Ok(json!(value * 2))
            }),
        )
        .unwrap();
    ManualDriver::new()
        .run_until(local.hydrate_external())
        .unwrap();
    local.activate().unwrap();
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    source
        .replace(&ctx, json!(2), ReplaceOptions::default())
        .unwrap();
    assert_eq!(source.snapshot().unwrap().value(), &json!(2));
    assert_eq!(projected.snapshot().unwrap().value(), &json!(2));
    assert_eq!(errors.borrow()[0].reason, "projection");
    source
        .replace(&ctx, json!(3), ReplaceOptions::default())
        .unwrap();
    assert_eq!(projected.snapshot().unwrap().value(), &json!(6));
}

#[test]
fn prepared_is_consumed_before_reentrant_observers_and_observer_errors_are_isolated() {
    let (graph, errors) = graph();
    let source = cell(&graph, "source", json!(1));
    let (ctx, _scope) = Context::new_root(1, 1, 1, Value::Null);
    let prepared = Rc::new(
        source
            .prepare_replace(&ctx, json!(2), ReplaceOptions::default())
            .unwrap(),
    );
    let calls = Rc::new(Counter::new(0));
    let (captured, candidate, context, observed) =
        (source.clone(), prepared.clone(), ctx.clone(), calls.clone());
    let _subscription = source
        .readable()
        .subscribe(move |snapshot| {
            if snapshot.value() == &json!(2) {
                assert_eq!(
                    captured.adopt(&context, &candidate).unwrap_err().reason,
                    "prepared_state"
                );
                observed.set(observed.get() + 1);
                return Err(RuntimeError::invalid("observer", "already committed"));
            }
            Ok(())
        })
        .unwrap();
    let committed = source.adopt(&ctx, &prepared).unwrap();
    assert_eq!(committed.value(), &json!(2));
    assert_eq!(calls.get(), 1);
    assert_eq!(errors.borrow()[0].reason, "observer");
}

#[test]
fn metadata_comparison_checks_versions_without_running_or_mutating_state() {
    let (graph, _) = graph();
    let cell = graph
        .declare_cell(
            "versioned".into(),
            StateDeclaration::new(json!({"type": "integer"}), json!(1)).version(2),
        )
        .unwrap();
    assert_eq!(
        cell.compare_declaration(&json!({"type": "integer"}), 2)
            .unwrap(),
        StateMetadataChange::Unchanged
    );
    assert_eq!(
        cell.compare_declaration(&json!({"type": "number"}), 3)
            .unwrap(),
        StateMetadataChange::MigrationRequired
    );
    assert_eq!(
        cell.compare_declaration(&json!({"type": "number"}), 2)
            .unwrap_err()
            .reason,
        "state_schema_version"
    );
    assert_eq!(
        cell.compare_declaration(&json!({"type": "integer"}), 1)
            .unwrap_err()
            .reason,
        "state_version"
    );
    assert_eq!(cell.version(), 2);
    assert_eq!(cell.snapshot().unwrap().value(), &json!(1));
}

#[test]
fn graph_termination_ends_external_hydration_without_retaining_its_authority() {
    let (external, _) = graph();
    let source = external
        .declare_cell(
            "pending".into(),
            StateDeclaration::with_initializer(Value::Bool(true), |_| {
                futures::future::pending().boxed_local()
            }),
        )
        .unwrap();
    let (local, _) = graph();
    let _forwarded = local.forward(source.readable()).unwrap();
    let mut future = local.hydrate_external();
    assert!(
        future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(Waker::noop()))
            .is_pending()
    );
    local.terminate(RuntimeError::aborted("disposed", "stopped"));
    assert_eq!(
        ManualDriver::new().run_until(future).unwrap_err().reason,
        "disposed"
    );
}

thread_local! {
    static WAKER_SOURCE: RefCell<Option<Readable>> = const { RefCell::new(None) };
}

struct WakerProbe {
    count: Arc<AtomicUsize>,
}
impl WakerProbe {
    fn inspect(&self) {
        WAKER_SOURCE.with(|source| {
            if let Some(source) = &*source.borrow() {
                let _ = source.snapshot();
            }
        });
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

// Each pointer owns one thread-safe Arc; clone increments it, wake/drop consume it.
// Reentrant local state access is thread-local, so the waker itself remains Send + Sync.
unsafe fn clone_probe(data: *const ()) -> RawWaker {
    let probe = unsafe { &*data.cast::<WakerProbe>() };
    probe.inspect();
    unsafe { Arc::increment_strong_count(data.cast::<WakerProbe>()) };
    RawWaker::new(data, &PROBE_VTABLE)
}
unsafe fn wake_probe(data: *const ()) {
    let probe = unsafe { Arc::from_raw(data.cast::<WakerProbe>()) };
    probe.inspect();
}
unsafe fn wake_probe_ref(data: *const ()) {
    unsafe { &*data.cast::<WakerProbe>() }.inspect();
}
unsafe fn drop_probe(data: *const ()) {
    let probe = unsafe { Arc::from_raw(data.cast::<WakerProbe>()) };
    probe.inspect();
}
static PROBE_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_probe, wake_probe, wake_probe_ref, drop_probe);

#[test]
fn hydration_waker_clone_replacement_and_drop_can_reenter_the_same_port() {
    let (graph, _) = graph();
    let source = graph
        .declare_cell(
            "pending".into(),
            StateDeclaration::with_initializer(Value::Bool(true), |_| {
                futures::future::pending().boxed_local()
            }),
        )
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    WAKER_SOURCE.with(|slot| *slot.borrow_mut() = Some(source.readable()));
    let probe = Arc::new(WakerProbe {
        count: count.clone(),
    });
    let raw = RawWaker::new(Arc::into_raw(probe).cast(), &PROBE_VTABLE);
    // The vtable maintains the Arc ownership contract for every raw pointer.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut context = std::task::Context::from_waker(&waker);
    let mut waiting = source.readable().await_hydrated();
    assert!(waiting.as_mut().poll(&mut context).is_pending());
    assert!(waiting.as_mut().poll(&mut context).is_pending());
    drop(waiting);
    assert!(count.load(Ordering::Relaxed) >= 4);
    drop(waker);
    WAKER_SOURCE.with(|slot| slot.borrow_mut().take());
}
