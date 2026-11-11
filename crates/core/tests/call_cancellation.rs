//! A caller cancels exactly the invocation it issued: that call and the calls it issued learn
//! it, nothing else in their root does, and every call stays physically counted until it ends.

use std::collections::BTreeSet;
use tembra_core::*;

fn fixture() -> (Host, Acquisition, Id) {
    let host = Host::new();
    let mut orb = Orb::new("scoped", 1);
    orb.isolated = true;
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
    host.drain();
    (host, reference, root)
}

fn cancelled(host: &Host) -> Vec<Id> {
    host.drain()
        .into_iter()
        .filter_map(|action| match action {
            Action::CancelCall(call) => Some(call),
            Action::CancelRoot(root) => panic!("root {root} was cancelled"),
            _ => None,
        })
        .collect()
}

#[test]
fn cancelling_a_call_reaches_its_descendants_and_nothing_else() {
    let (host, reference, root) = fixture();
    let step = |parent: Option<Id>| {
        host.begin_call_with_context(reference.reference, root, "step", 0, parent)
            .unwrap()
    };
    let caller = step(None);
    let issued = step(Some(caller.id));
    let child = step(Some(issued.id));
    let grandchild = step(Some(child.id));
    let sibling = step(Some(caller.id));
    let unrelated = step(None);

    assert_eq!(
        host.cancel_call(issued.id).unwrap(),
        [issued.id, child.id, grandchild.id]
    );
    assert_eq!(cancelled(&host), [issued.id, child.id, grandchild.id]);
    for call in [issued.id, child.id, grandchild.id] {
        assert!(host.call_cancelled(call));
        assert_eq!(host.check_call(call).unwrap_err().code, Code::Cancelled);
    }
    for call in [caller.id, sibling.id, unrelated.id] {
        assert!(!host.call_cancelled(call));
        host.check_call(call).unwrap();
    }
    host.check_root(root).unwrap();

    // Idempotent: nothing is announced twice.
    assert!(host.cancel_call(issued.id).unwrap().is_empty());
    assert!(host.cancel_call(child.id).unwrap().is_empty());
    assert!(cancelled(&host).is_empty());

    // Logical only: every call is still counted until it physically ends.
    assert_eq!(host.diagnostics().calls, 6);
    let caller = caller.id;
    for call in [
        grandchild.id,
        child.id,
        issued.id,
        sibling.id,
        caller,
        unrelated.id,
    ] {
        host.finish_call(call).unwrap();
    }
    assert_eq!(host.diagnostics().calls, 0);
    assert_eq!(host.cancel_call(caller).unwrap_err().code, Code::Gone);
    host.end_root(root).unwrap();
}

#[test]
fn a_cancelled_call_derives_no_fresh_authority() {
    let (host, reference, root) = fixture();
    let issued = host.begin_call(reference.reference, root, "step").unwrap();
    let frame = host.enter_execution(issued.id, 0).unwrap();
    host.cancel_call(issued.id).unwrap();
    let refused =
        |result: Result<Invocation>| assert_eq!(result.unwrap_err().code, Code::Cancelled);
    refused(host.begin_call_with_context(
        reference.reference,
        root,
        "step",
        frame,
        Some(issued.id),
    ));
    refused(host.begin_operation_with_context(
        issued.orb,
        root,
        7,
        Some(issued.object),
        frame,
        Some(issued.id),
    ));
    assert_eq!(
        host.enter_execution(issued.id, frame).unwrap_err().code,
        Code::Cancelled
    );
    let key: ContextKey<i64> = ContextKey::new("test/number", 1);
    assert_eq!(
        host.derive_context(root, frame, Some(issued.id), key.contract(), 1.into_value())
            .unwrap_err()
            .code,
        Code::Cancelled
    );
    // The root itself still admits fresh work.
    let fresh = host.begin_call(reference.reference, root, "step").unwrap();
    host.finish_call(fresh.id).unwrap();
    host.finish_call(issued.id).unwrap();
    host.end_root(root).unwrap();
    assert_eq!(host.diagnostics().calls, 0);
}

#[test]
fn a_call_outliving_its_issuer_stays_reachable_from_the_issuers_ancestor() {
    let (host, reference, root) = fixture();
    let step = |parent: Option<Id>| {
        host.begin_call_with_context(reference.reference, root, "step", 0, parent)
            .unwrap()
    };
    let ancestor = step(None);
    let issuer = step(Some(ancestor.id));
    let abandoned = step(Some(issuer.id));
    // The issuer ended while the call it issued keeps running.
    host.finish_call(issuer.id).unwrap();
    assert_eq!(
        host.cancel_call(ancestor.id).unwrap(),
        [ancestor.id, abandoned.id]
    );
    assert_eq!(
        host.check_call(abandoned.id).unwrap_err().code,
        Code::Cancelled
    );
    host.finish_call(abandoned.id).unwrap();
    host.finish_call(ancestor.id).unwrap();
    host.end_root(root).unwrap();
}

#[test]
fn a_parent_from_another_root_or_an_ended_parent_is_refused() {
    let (host, reference, root) = fixture();
    let other_root = host.begin_root("bob").unwrap();
    let foreign = host
        .begin_call(reference.reference, other_root, "step")
        .unwrap();
    assert!(
        host.begin_call_with_context(reference.reference, root, "step", 0, Some(foreign.id))
            .is_err()
    );
    let ended = host.begin_call(reference.reference, root, "step").unwrap();
    host.finish_call(ended.id).unwrap();
    assert_eq!(
        host.begin_call_with_context(reference.reference, root, "step", 0, Some(ended.id))
            .unwrap_err()
            .code,
        Code::Gone
    );
    host.finish_call(foreign.id).unwrap();
    host.end_root(other_root).unwrap();
    host.end_root(root).unwrap();
}

#[test]
fn root_cancellation_is_unchanged_and_announces_no_call() {
    let (host, reference, root) = fixture();
    let issued = host.begin_call(reference.reference, root, "step").unwrap();
    host.cancel_root(root).unwrap();
    assert_eq!(host.drain(), [Action::CancelRoot(root)]);
    assert!(!host.call_cancelled(issued.id));
    host.finish_call(issued.id).unwrap();
    host.end_root(root).unwrap();
}
