use super::*;
use crate::{
    GetOptions, Host, HostOptions, Implementation, ManualDriver, ServeOptions,
    contracts::{Contract, Member, ValueSchema},
    rpc::{ConnectionOptions, MemoryLinkOptions, memory_link},
};
use futures::FutureExt;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn settled_origin_index_is_bounded_even_when_expired_contexts_are_retained() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("root-index", json!(null)), driver.clone());
    let mut expired = Vec::new();
    for _ in 0..256 {
        let context = driver
            .run_until(host.root(|context| async move { Ok(context) }.boxed_local()))
            .unwrap();
        assert_eq!(context.check(host.id()).unwrap_err().reason, "expired");
        expired.push(context);
    }
    assert_eq!(expired.len(), 256);
    assert!(host.inner.roots.epoch_idle(1));
    assert_eq!(host.inner.roots.local.borrow().len(), 0);

    let (live, scope) = host.inner.new_root(json!(null));
    assert!(!host.inner.roots.epoch_idle(1));
    assert_eq!(host.inner.roots.local.borrow().len(), 1);
    drop(scope);
    assert_eq!(live.check(host.id()).unwrap_err().reason, "expired");
    assert!(host.inner.roots.epoch_idle(1));
    assert_eq!(host.inner.roots.local.borrow().len(), 0);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn ordinary_native_calls_retire_each_root_without_any_reload() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("native-root-index", json!(null)),
        driver.clone(),
    );
    let token = Rc::new(
        Contract::local(
            "native-root-owner",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("root.argument@1".into())),
                    result: Some(ValueSchema::Native("root.result@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        token.clone(),
        Rc::new(|_, _, _| {
            Ok(
                Implementation::new().method("read", |value: Rc<usize>, context| {
                    async move { Ok((value, context)) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(token, json!({}), GetOptions::default()))
        .unwrap();
    let object = reference.claim.local_object().unwrap();
    let value = Rc::new(17usize);
    let mut expired = Vec::new();
    for _ in 0..256 {
        let (returned, context) = driver
            .run_until(reference.call::<Rc<usize>, (Rc<usize>, Context)>(
                "read",
                value.clone(),
                None,
            ))
            .unwrap();
        assert!(
            Rc::ptr_eq(&returned, &value),
            "native dispatch preserves the actual value"
        );
        assert_eq!(context.code_version(), 1);
        assert_eq!(context.check(host.id()).unwrap_err().reason, "expired");
        assert_eq!(object.invocations.get(), 0);
        assert_eq!(
            host.inner.roots.local.borrow().len(),
            0,
            "normal root settlement prunes the index immediately, without epoch retirement"
        );
        expired.push(context);
    }
    assert_eq!(expired.len(), 256);
    assert_eq!(host.inner.epochs.current().version.get(), 1);
    assert!(host.inner.epochs.previous().is_none());
    assert!(host.inner.roots.epoch_idle(1));
    reference.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn imported_index_retains_exact_carrier_and_physical_entry_until_both_end() {
    let driver = ManualDriver::new();
    let parent = Host::new(
        HostOptions::new("index-parent", json!(null)),
        driver.clone(),
    );
    let child = Host::new(HostOptions::new("index-child", json!(null)), driver.clone());
    parent
        .set_rpc_identity(
            RootOrigin::new("index-parent", "00000000000000000000000000000001").unwrap(),
        )
        .unwrap();
    child
        .set_rpc_identity(
            RootOrigin::new("index-child", "00000000000000000000000000000002").unwrap(),
        )
        .unwrap();
    let (parent_link, child_link, _) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let incoming = parent
        .accept_child(
            parent_link,
            "index-child",
            json!(null),
            ConnectionOptions::default(),
        )
        .unwrap();
    let outgoing = child
        .connect_parent(
            child_link,
            "index-parent",
            json!(null),
            ConnectionOptions::default(),
        )
        .unwrap();
    driver.run_until(incoming.ready()).unwrap();
    driver.run_until(outgoing.ready()).unwrap();
    let registry = &parent.inner.roots;
    for (sequence, carrier_first) in [(1, true), (2, false)] {
        let wire = RootRef {
            identity: RootId {
                origin: RootOrigin::new("index-child", "00000000000000000000000000000002").unwrap(),
                sequence: PositiveId::new(sequence).unwrap(),
            },
            code_version: PositiveId::new(1).unwrap(),
        };
        let entry = registry
            .enter(Some(&wire), incoming.id(), json!(null))
            .unwrap();
        let retained_context = entry.context().clone();
        assert_eq!(registry.local.borrow().len(), 1);
        assert_eq!(registry.incoming.get(), 1);
        assert!(!registry.epoch_idle(1));
        if carrier_first {
            registry.carrier_end(incoming.id(), &wire.identity);
            assert_eq!(registry.local.borrow().len(), 1);
            assert_eq!(registry.incoming.get(), 1);
            assert!(!registry.epoch_idle(1));
            drop(entry);
        } else {
            drop(entry);
            assert_eq!(registry.incoming.get(), 0);
            assert_eq!(registry.local.borrow().len(), 1);
            assert!(!registry.epoch_idle(1));
            registry.carrier_end(incoming.id(), &wire.identity);
        }
        assert_eq!(
            retained_context.check(parent.id()).unwrap_err().reason,
            "expired"
        );
        assert_eq!(registry.local.borrow().len(), 0);
        assert_eq!(registry.incoming.get(), 0);
        assert!(registry.epoch_idle(1));
        assert_eq!(
            registry
                .enter(Some(&wire), incoming.id(), json!(null))
                .err()
                .unwrap()
                .reason,
            "root_ended"
        );
    }
    driver.run_until(parent.close()).unwrap();
    driver.run_until(child.close()).unwrap();
}

#[test]
fn settled_origin_removal_does_not_release_a_detached_physical_invocation() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("physical-index", json!(null)),
        driver.clone(),
    );
    let token = Rc::new(
        Contract::local(
            "physical-owner",
            1,
            json!({}),
            BTreeMap::from([(
                "hold".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("physical.unit@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let lease = Rc::new(RefCell::new(None));
    let captured = lease.clone();
    host.serve(
        token.clone(),
        Rc::new(move |_, _, declaration| {
            let owner = declaration.invocation_owner();
            let captured = captured.clone();
            Ok(
                Implementation::new().method("hold", move |(): (), context| {
                    *captured.borrow_mut() = Some(owner.retain(&context).unwrap());
                    async { Ok(()) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(token, json!({}), GetOptions::default()))
        .unwrap();
    driver
        .run_until(reference.call::<(), ()>("hold", (), None))
        .unwrap();
    let object = reference.claim.local_object().unwrap();
    assert_eq!(host.inner.roots.local.borrow().len(), 0);
    assert_eq!(object.invocations.get(), 1);
    let gate = host
        .inner
        .roots
        .begin_gate(std::slice::from_ref(object))
        .unwrap();
    assert!(gate.drained().now_or_never().is_none());
    let detached = lease.borrow_mut().take();
    drop(detached);
    driver.run_until(gate.drained()).unwrap();
    assert_eq!(object.invocations.get(), 0);
    gate.open();
    reference.release();
    driver.run_until(host.close()).unwrap();
}
