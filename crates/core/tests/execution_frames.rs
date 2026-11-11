//! Private execution ancestry follows the existing retained context-frame lifetime.
use std::collections::BTreeSet;
use tembra_core::*;

fn fixture() -> (Host, Acquisition, Id) {
    let host = Host::new();
    let mut orb = Orb::new("isolated", 1);
    orb.isolated = true;
    orb.services.push(Service::new("isolated", ["step"]));
    let reload = host.prepare(vec![orb], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let reference = host
        .acquire(
            Address {
                service: "isolated".into(),
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
fn private_ancestry_rejects_reentry_without_registering_application_keys() {
    let (host, reference, root) = fixture();
    let first = host.begin_call(reference.reference, root, "step").unwrap();
    let frame = host.enter_execution(first.id, 0).unwrap();
    assert_eq!(host.context_stats().2, 0);
    let trace = ContextKey::<String>::new("trace", 1);
    let inherited = host
        .derive_context(
            root,
            frame,
            Some(first.id),
            trace.contract(),
            "a".to_string().into_value(),
        )
        .unwrap();
    let second = host
        .begin_call_with_context(reference.reference, root, "step", inherited, None)
        .unwrap();
    let before = host.context_stats();
    assert!(
        host.enter_execution(second.id, inherited)
            .unwrap_err()
            .message
            .contains("causal reentry")
    );
    assert_eq!(host.context_stats(), before);
    assert_eq!(
        host.context_value(root, inherited, &trace.contract())
            .unwrap(),
        Some("a".to_string().into_value())
    );
    host.finish_call(first.id).unwrap();
    assert!(host.context_stats().0 > 0);
    assert_eq!(
        host.context_value(root, inherited, &trace.contract())
            .unwrap(),
        Some("a".to_string().into_value())
    );
    assert!(host.enter_execution(second.id, inherited).is_err());
    host.finish_call(second.id).unwrap();
    assert_eq!(host.context_stats().0, 0);
    host.end_root(root).unwrap();
}

#[test]
fn independent_invocations_and_cancelled_or_expired_frames_keep_their_lifetimes() {
    let (host, reference, root) = fixture();
    let first = host.begin_call(reference.reference, root, "step").unwrap();
    let a = host.enter_execution(first.id, 0).unwrap();
    let second = host.begin_call(reference.reference, root, "step").unwrap();
    let b = host.enter_execution(second.id, 0).unwrap();
    assert_ne!(a, b);
    let other_root = host.begin_root("bob").unwrap();
    let other = host
        .begin_call(reference.reference, other_root, "step")
        .unwrap();
    assert!(host.enter_execution(other.id, a).is_err());
    host.cancel_root(root).unwrap();
    assert!(host.enter_execution(second.id, 0).is_err());
    host.finish_call(first.id).unwrap();
    assert!(host.enter_execution(first.id, 0).is_err());
    host.finish_call(second.id).unwrap();
    host.end_root(root).unwrap();
    host.finish_call(other.id).unwrap();
    host.end_root(other_root).unwrap();
    assert_eq!(host.context_stats().0, 0);
}

#[test]
fn trusted_intermediaries_preserve_the_private_heap_identity() {
    let host = Host::new();
    let mut isolated = Orb::new("isolated", 1);
    isolated.isolated = true;
    isolated.services.push(Service::new("isolated", ["step"]));
    let mut trusted = Orb::new("trusted", 1);
    trusted.services.push(Service::new("trusted", ["step"]));
    let reload = host.prepare(vec![isolated, trusted], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let acquire = |service: &str| {
        let reference = host
            .acquire(
                Address {
                    service: service.into(),
                    key: vec!["main".into()],
                    principal: "alice".into(),
                },
                1,
                BTreeSet::new(),
            )
            .unwrap();
        host.ready(reference.object).unwrap();
        reference
    };
    let a = acquire("isolated");
    let b = acquire("trusted");
    let root = host.begin_root("alice").unwrap();
    let first = host.begin_call(a.reference, root, "step").unwrap();
    let frame = host.enter_execution(first.id, 0).unwrap();
    let middle = host
        .begin_call_with_context(b.reference, root, "step", frame, None)
        .unwrap();
    assert!(host.enter_execution(middle.id, frame).is_err());
    let bounce = host
        .begin_call_with_context(a.reference, root, "step", middle.context, None)
        .unwrap();
    assert!(host.enter_execution(bounce.id, bounce.context).is_err());
    for call in [bounce.id, middle.id, first.id] {
        host.finish_call(call).unwrap();
    }
    host.end_root(root).unwrap();
    assert_eq!(host.context_stats().0, 0);
}
