//! Scalar scope checks preserve authority and errors without allocating an input tree.
use serde_json::Value;
use tembra_bridge::{Kernel, core::*, input, wire::Handle};

#[test]
fn scalar_and_typed_checks_share_root_cancellation_and_identity_rules() {
    let kernel = Kernel::new();
    let root = kernel.host.begin_root("alice").unwrap();
    assert_eq!(kernel.check(root as f64, None), None);
    kernel
        .run(input::CheckContext {
            root: Handle(root),
            call: None,
        })
        .unwrap();
    for value in [f64::NAN, f64::INFINITY, -1.0, 0.5, (MAX_ID + 1) as f64] {
        let error: Value = serde_json::from_str(&kernel.check(value, None).unwrap()).unwrap();
        assert_eq!(error["code"], "invalid");
        assert_eq!(error["reason"], "metadata");
    }
    let before = kernel.host.diagnostics();
    let error: Value =
        serde_json::from_str(&kernel.check(root as f64, Some(0.0)).unwrap()).unwrap();
    assert_eq!(error["reason"], "call");
    assert_eq!(kernel.host.diagnostics(), before);
    kernel.host.cancel_root(root).unwrap();
    let scalar: Value = serde_json::from_str(&kernel.check(root as f64, None).unwrap()).unwrap();
    let typed = kernel
        .run(input::CheckContext {
            root: Handle(root),
            call: None,
        })
        .unwrap_err();
    assert_eq!(scalar["code"], typed.code.as_str());
    assert_eq!(scalar["reason"], typed.reason);
    kernel.host.end_root(root).unwrap();
    assert!(kernel.check(root as f64, None).is_some());
}

#[test]
fn root_settlement_returns_actions_once_and_does_not_end_physical_work() {
    let kernel = Kernel::new();
    let mut orb = Orb::new("calls", 1);
    orb.services.push(Service::new("calls", ["read"]));
    let prepared = kernel.host.prepare(vec![orb], vec![]).unwrap();
    kernel.host.commit(prepared.id, vec![]).unwrap();
    let reference = kernel
        .host
        .acquire(
            Address {
                service: "calls".into(),
                key: vec!["main".into()],
                principal: "alice".into(),
            },
            1,
            Default::default(),
        )
        .unwrap();
    kernel.host.ready(reference.object).unwrap();
    let root = kernel.host.begin_root("alice").unwrap();
    let call = kernel
        .host
        .begin_call(reference.reference, root, "read")
        .unwrap();
    let result = kernel
        .run(input::EndRootBatch { root: Handle(root) })
        .unwrap();
    assert!(result.actions.is_empty());
    assert_eq!(kernel.host.drain().len(), 0);
    assert!(kernel.host.check_root(root).is_err());
    kernel.host.invocation(call.id).unwrap();
    assert_eq!(kernel.host.diagnostics().calls, 1);
    kernel.host.finish_call(call.id).unwrap();
    assert_eq!(kernel.host.diagnostics().calls, 0);
}
