//! Typed frame isolation, bounded retention and physical settlement.
use std::{collections::BTreeSet, rc::Rc};
use tembra_core::*;
const TRACE: ContextKey<Rc<str>> = ContextKey::new("test/trace", 1);

#[test]
fn frames_are_per_call_immutable_typed_and_retained_until_physical_settlement() {
    let host = Host::new();
    let mut orb = Orb::new("test", 1);
    orb.services.push(Service::new("test", ["echo"]));
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let object = host
        .acquire(
            Address {
                service: "test".into(),
                key: vec!["main".into()],
                principal: "alice".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap();
    host.ready(object.object).unwrap();
    let root = host.begin_root("alice").unwrap();
    let left = host
        .derive_context(
            root,
            0,
            None,
            TRACE.contract(),
            ContextValue::Text("left".into()),
        )
        .unwrap();
    let right = host
        .derive_context(
            root,
            0,
            None,
            TRACE.contract(),
            ContextValue::Text("right".into()),
        )
        .unwrap();
    let first = host
        .begin_call_with_context(object.reference, root, "echo", left)
        .unwrap();
    let second = host
        .begin_call_with_context(object.reference, root, "echo", right)
        .unwrap();
    assert_eq!(
        host.context_value(root, 0, &TRACE.contract()).unwrap(),
        None
    );
    assert_eq!(
        host.context_value(root, first.context, &TRACE.contract())
            .unwrap(),
        Some(ContextValue::Text("left".into()))
    );
    assert_eq!(
        host.context_value(root, second.context, &TRACE.contract())
            .unwrap(),
        Some(ContextValue::Text("right".into()))
    );
    assert_eq!(first.code_version, second.code_version);
    assert_eq!(host.root_principal(root).unwrap(), "alice");
    let mut wrong = TRACE.contract();
    wrong.kind = ContextKind::Integer;
    assert!(host.context_value(root, left, &wrong).is_err());
    let before = host.context_stats();
    assert!(
        host.derive_context(root, left, None, wrong, ContextValue::Integer(1))
            .is_err()
    );
    assert_eq!(host.context_stats(), before);
    let foreign = host.begin_root("alice").unwrap();
    assert!(
        host.begin_call_with_context(object.reference, foreign, "echo", left)
            .is_err()
    );
    host.end_root(foreign).unwrap();
    host.end_root(root).unwrap();
    assert_eq!(host.context_stats().0, 2);
    assert!(host.context_value(root, left, &TRACE.contract()).is_err());
    host.finish_call(first.id).unwrap();
    assert_eq!(host.context_stats().0, 2);
    host.finish_call(second.id).unwrap();
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn cancellation_and_ended_invocations_do_not_extend_derivation_validity() {
    let host = Host::new();
    let root = host.begin_root("alice").unwrap();
    let frame = host
        .derive_context(
            root,
            0,
            None,
            TRACE.contract(),
            ContextValue::Text("trace".into()),
        )
        .unwrap();
    host.cancel_root(root).unwrap();
    assert_eq!(
        host.derive_context(
            root,
            frame,
            None,
            TRACE.contract(),
            ContextValue::Text("other".into())
        )
        .unwrap_err()
        .code,
        Code::Cancelled
    );
    assert_eq!(host.context_stats().0, 1);
    host.end_root(root).unwrap();
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn frames_bound_depth_and_retain_only_the_ending_roots_bindings() {
    let host = Host::new();
    let first = host.begin_root("a").unwrap();
    let other = host.begin_root("b").unwrap();
    let mut parent = 0;
    for _ in 0..64 {
        parent = host
            .derive_context(
                first,
                parent,
                None,
                TRACE.contract(),
                ContextValue::Text("x".into()),
            )
            .unwrap();
    }
    assert_eq!(
        host.derive_context(
            first,
            parent,
            None,
            TRACE.contract(),
            ContextValue::Text("x".into())
        )
        .unwrap_err()
        .code,
        Code::Exhausted
    );
    host.derive_context(
        other,
        0,
        None,
        TRACE.contract(),
        ContextValue::Text("b".into()),
    )
    .unwrap();
    host.end_root(first).unwrap();
    assert_eq!(host.context_stats().0, 1);
    host.end_root(other).unwrap();
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn owner_leases_are_released_outside_the_kernel_borrow() {
    use std::cell::Cell;
    struct Lease {
        host: Host,
        dropped: Rc<Cell<bool>>,
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            assert_eq!(self.host.context_stats().0, 0);
            self.dropped.set(true);
        }
    }
    let host = Host::new();
    let root = host.begin_root("owner").unwrap();
    let dropped = Rc::new(Cell::new(false));
    host.derive_context(
        root,
        0,
        None,
        ContextContract {
            name: "owner/reference".into(),
            version: 1,
            kind: ContextKind::Opaque,
        },
        ContextValue::Opaque(OpaqueContextValue::new(
            1,
            Lease {
                host: host.clone(),
                dropped: dropped.clone(),
            },
        )),
    )
    .unwrap();
    assert!(!dropped.get());
    host.end_root(root).unwrap();
    assert!(dropped.get());
}

#[test]
fn scalar_keys_share_exact_safe_integer_and_boolean_contracts() {
    let host = Host::new();
    let root = host.begin_root("scalar").unwrap();
    let integer: ContextKey<i64> = ContextKey::new("scalar/integer", 1);
    let boolean: ContextKey<bool> = ContextKey::new("scalar/boolean", 1);
    let frame = host
        .derive_context(
            root,
            0,
            None,
            integer.contract(),
            9_007_199_254_740_991_i64.into_value(),
        )
        .unwrap();
    let frame = host
        .derive_context(root, frame, None, boolean.contract(), true.into_value())
        .unwrap();
    assert_eq!(
        i64::from_value(
            host.context_value(root, frame, &integer.contract())
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        9_007_199_254_740_991
    );
    assert!(
        bool::from_value(
            host.context_value(root, frame, &boolean.contract())
                .unwrap()
                .unwrap()
        )
        .unwrap()
    );
    host.end_root(root).unwrap();
}

#[test]
fn absent_bindings_are_none_even_if_another_context_registered_a_different_kind() {
    let host = Host::new();
    let root = host.begin_root("reader").unwrap();
    host.derive_context(
        root,
        0,
        None,
        TRACE.contract(),
        ContextValue::Text("x".into()),
    )
    .unwrap();
    let mut wrong = TRACE.contract();
    wrong.kind = ContextKind::Integer;
    assert_eq!(host.context_value(root, 0, &wrong).unwrap(), None);
    host.end_root(root).unwrap();
}
