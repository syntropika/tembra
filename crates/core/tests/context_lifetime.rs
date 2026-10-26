//! Invocation frames retain only their live creators, consumers and ancestors.

use std::{collections::BTreeSet, rc::Rc};
use tembra_core::*;

const TRACE: ContextKey<Rc<str>> = ContextKey::new("test/trace", 1);

fn invocation_fixture() -> (Host, Acquisition, Id) {
    let host = Host::new();
    let mut orb = Orb::new("scoped", 1);
    orb.services.push(Service::new("scoped", ["step"]));
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let reference = host
        .acquire(
            Address {
                service: "scoped".into(),
                key: vec!["main".into()],
                principal: "alice".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap();
    host.ready(reference.object).unwrap();
    let root = host.begin_root("alice").unwrap();
    (host, reference, root)
}

#[test]
fn twenty_thousand_serial_invocations_reclaim_frames_while_outer_call_stays_live() {
    let (host, reference, root) = invocation_fixture();
    let outer = host.begin_call(reference.reference, root, "step").unwrap();
    for _ in 0..20_000 {
        let producer = host.begin_call(reference.reference, root, "step").unwrap();
        let frame = host
            .derive_context(
                root,
                0,
                Some(producer.id),
                TRACE.contract(),
                "step".to_string().into_value(),
            )
            .unwrap();
        let consumer = host
            .begin_call_with_context(reference.reference, root, "step", frame)
            .unwrap();
        assert_eq!(
            host.context_value(root, frame, &TRACE.contract()).unwrap(),
            Some("step".to_string().into_value())
        );
        host.finish_call(consumer.id).unwrap();
        host.finish_call(producer.id).unwrap();
        assert_eq!(host.context_stats().0, 0);
        assert!(
            host.begin_call_with_context(reference.reference, root, "step", frame)
                .is_err()
        );
    }
    assert_eq!(host.context_stats().1, 1);
    host.invocation(outer.id).unwrap();
    host.finish_call(outer.id).unwrap();
    host.end_root(root).unwrap();
}

#[test]
fn descendant_outlives_both_creators_and_preserves_inherited_keys() {
    let (host, reference, root) = invocation_fixture();
    let first = host.begin_call(reference.reference, root, "step").unwrap();
    let parent = host
        .derive_context(
            root,
            0,
            Some(first.id),
            TRACE.contract(),
            "parent".to_string().into_value(),
        )
        .unwrap();
    let second = host
        .begin_call_with_context(reference.reference, root, "step", parent)
        .unwrap();
    let number: ContextKey<i64> = ContextKey::new("test/number", 1);
    let child = host
        .derive_context(
            root,
            parent,
            Some(second.id),
            number.contract(),
            42.into_value(),
        )
        .unwrap();
    let third = host
        .begin_call_with_context(reference.reference, root, "step", child)
        .unwrap();
    host.finish_call(first.id).unwrap();
    host.finish_call(second.id).unwrap();
    assert_eq!(host.context_stats().0, 2);
    assert_eq!(
        host.context_value(root, child, &TRACE.contract()).unwrap(),
        Some("parent".to_string().into_value())
    );
    assert_eq!(
        host.context_value(root, child, &number.contract()).unwrap(),
        Some(42.into_value())
    );
    host.finish_call(third.id).unwrap();
    assert_eq!(host.context_stats().0, 0);
    host.end_root(root).unwrap();
}

#[test]
fn rejected_admission_does_not_pin_and_siblings_do_not_release_each_other() {
    let (host, reference, root) = invocation_fixture();
    let left = host.begin_call(reference.reference, root, "step").unwrap();
    let right = host.begin_call(reference.reference, root, "step").unwrap();
    let a = host
        .derive_context(
            root,
            0,
            Some(left.id),
            TRACE.contract(),
            "left".to_string().into_value(),
        )
        .unwrap();
    let b = host
        .derive_context(
            root,
            0,
            Some(right.id),
            TRACE.contract(),
            "right".to_string().into_value(),
        )
        .unwrap();
    assert!(
        host.begin_call_with_context(reference.reference, root, "absent", a)
            .is_err()
    );
    host.finish_call(left.id).unwrap();
    assert!(host.context_value(root, a, &TRACE.contract()).is_err());
    assert_eq!(
        host.context_value(root, b, &TRACE.contract()).unwrap(),
        Some("right".to_string().into_value())
    );
    host.finish_call(right.id).unwrap();
    assert_eq!(host.context_stats().0, 0);
    host.end_root(root).unwrap();
}

#[test]
fn root_bindings_keep_their_lifetime_and_live_frame_quota_remains_enforced() {
    let (host, reference, root) = invocation_fixture();
    let root_frame = host
        .derive_context(
            root,
            0,
            None,
            TRACE.contract(),
            "root".to_string().into_value(),
        )
        .unwrap();
    let producer = host
        .begin_call_with_context(reference.reference, root, "step", root_frame)
        .unwrap();
    for _ in 0..4095 {
        host.derive_context(
            root,
            root_frame,
            Some(producer.id),
            TRACE.contract(),
            "short".to_string().into_value(),
        )
        .unwrap();
    }
    assert_eq!(
        host.derive_context(
            root,
            root_frame,
            Some(producer.id),
            TRACE.contract(),
            "excess".to_string().into_value()
        )
        .unwrap_err()
        .code,
        Code::Exhausted
    );
    host.finish_call(producer.id).unwrap();
    assert_eq!(host.context_stats().0, 1);
    assert_eq!(
        host.context_value(root, root_frame, &TRACE.contract())
            .unwrap(),
        Some("root".to_string().into_value())
    );
    let next = host.begin_call(reference.reference, root, "step").unwrap();
    host.derive_context(
        root,
        root_frame,
        Some(next.id),
        TRACE.contract(),
        "next".to_string().into_value(),
    )
    .unwrap();
    host.finish_call(next.id).unwrap();
    assert_eq!(host.context_stats().0, 1);
    host.end_root(root).unwrap();
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn cancellation_and_root_close_preserve_frames_until_physical_descendant_settlement() {
    let (host, reference, root) = invocation_fixture();
    let first = host.begin_call(reference.reference, root, "step").unwrap();
    let frame = host
        .derive_context(
            root,
            0,
            Some(first.id),
            TRACE.contract(),
            "pending".to_string().into_value(),
        )
        .unwrap();
    let child = host
        .begin_call_with_context(reference.reference, root, "step", frame)
        .unwrap();
    host.cancel_root(root).unwrap();
    host.end_root(root).unwrap();
    host.finish_call(first.id).unwrap();
    assert_eq!(host.context_stats().0, 1);
    host.finish_call(child.id).unwrap();
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn owned_operations_retain_inherited_frames_after_service_creator_settles() {
    let (host, reference, root) = invocation_fixture();
    let first = host.begin_call(reference.reference, root, "step").unwrap();
    let frame = host
        .derive_context(
            root,
            0,
            Some(first.id),
            TRACE.contract(),
            "owned".to_string().into_value(),
        )
        .unwrap();
    let operation = host
        .begin_operation_with_context(first.orb, root, 99, Some(first.object), frame)
        .unwrap();
    host.finish_call(first.id).unwrap();
    assert_eq!(host.context_stats().0, 1);
    host.finish_call(operation.id).unwrap();
    assert_eq!(host.context_stats().0, 0);
    host.end_root(root).unwrap();
}

#[test]
fn reclaimed_opaque_destructors_reenter_after_call_bookkeeping_settles() {
    use std::cell::Cell;
    struct Lease(Host, Rc<Cell<bool>>);
    impl Drop for Lease {
        fn drop(&mut self) {
            assert_eq!(self.0.context_stats().0, 0);
            self.1.set(true);
        }
    }
    let (host, reference, root) = invocation_fixture();
    let producer = host.begin_call(reference.reference, root, "step").unwrap();
    let dropped = Rc::new(Cell::new(false));
    let key: ContextKey<OpaqueContextValue> = ContextKey::new("test/opaque", 1);
    host.derive_context(
        root,
        0,
        Some(producer.id),
        key.contract(),
        OpaqueContextValue::new(1, Lease(host.clone(), dropped.clone())).into_value(),
    )
    .unwrap();
    host.finish_call(producer.id).unwrap();
    assert!(dropped.get());
    host.check_root(root).unwrap();
    host.end_root(root).unwrap();
}
