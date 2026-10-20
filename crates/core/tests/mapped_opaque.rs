use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

struct Nominal(Cell<u32>);
struct Wrapped(Rc<Nominal>);
fn token(name: &str, version: u64) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            version,
            json!({}),
            BTreeMap::from([(
                "native".into(),
                Member::Opaque {
                    id: "example.nominal@1".into(),
                },
            )]),
        )
        .unwrap(),
    )
}
fn empty(name: &str) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}

#[test]
fn mapped_opaque_preserves_nominal_identity_through_adapter_and_mapper_refusal() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("mapped", json!(null)), driver.clone());
    let provider = token("mapped", 2);
    let consumer = token("mapped", 1);
    let native = Rc::new(Nominal(Cell::new(7)));
    let reject = Rc::new(Cell::new(true));
    let calls = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let saved = Rc::new(RefCell::new(None::<WeakResource<Wrapped>>));
    let (value, refusal, mapped, closed, resource_slot) = (
        native.clone(),
        reject.clone(),
        calls.clone(),
        closes.clone(),
        saved.clone(),
    );
    let mut options = ServeOptions::default();
    options
        .accepts
        .insert(1, Rc::new(|implementation| Ok((*implementation).clone())));
    host.serve(
        provider.clone(),
        Rc::new(move |_, _, declaration| {
            let value = value.clone();
            let closed = closed.clone();
            let resource = declaration.resource(
                "wrapped",
                ResourceDeclaration::new(move |_| {
                    let value = value.clone();
                    async move { Ok(Wrapped(value)) }.boxed_local()
                })
                .with_close(move |value| {
                    assert_eq!(value.0.0.get(), 19);
                    closed.set(closed.get() + 1);
                    async { Ok(()) }.boxed_local()
                }),
            )?;
            *resource_slot.borrow_mut() = Some(resource.downgrade());
            let (refusal, mapped) = (refusal.clone(), mapped.clone());
            Ok(
                Implementation::new().map_opaque("native", resource, move |value| {
                    mapped.set(mapped.get() + 1);
                    if refusal.get() {
                        return Err(RuntimeError::invalid(
                            "decode",
                            "nominal projection refused",
                        ));
                    }
                    Ok(value.0.clone())
                }),
            )
        }),
        options,
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(provider, json!({}), GetOptions::default()))
        .unwrap();
    let legacy = driver
        .run_until(host.get(consumer, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(
        calls.get(),
        0,
        "registration and view validation never invoke the mapper"
    );
    assert_eq!(
        reference.opaque::<Nominal>("native").err().unwrap().reason,
        "decode"
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(closes.get(), 0);
    let resource = saved.borrow().as_ref().unwrap().upgrade().unwrap();
    assert!(Rc::ptr_eq(&resource.value().unwrap().0, &native));
    reject.set(false);
    let current_value = reference.opaque::<Nominal>("native").unwrap();
    let legacy_value = legacy.opaque::<Nominal>("native").unwrap();
    assert!(Rc::ptr_eq(&current_value, &native));
    assert!(Rc::ptr_eq(&legacy_value, &native));
    current_value.0.set(19);
    assert_eq!(legacy_value.0.get(), 19);
    let before = calls.get();
    let during_factory = reference.clone();
    let observed = calls.clone();
    let helper = empty("factory-probe");
    host.serve(
        helper.clone(),
        Rc::new(move |_, _, _| {
            let error = during_factory.opaque::<Nominal>("native").err().unwrap();
            assert_eq!(
                (error.code, error.reason.as_str()),
                (ErrorCode::NotReady, "factory")
            );
            assert_eq!(observed.get(), before);
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let helper_ref = driver
        .run_until(host.get(helper, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(calls.get(), before);
    driver.run_until(host.close()).unwrap();
    assert_eq!(closes.get(), 1);
    assert!(reference.opaque::<Nominal>("native").is_err());
    assert_eq!(calls.get(), before);
    helper_ref.release();
}

#[test]
fn mapped_getter_never_calls_mapper_for_pending_or_closed_existing_resource() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("mapped", json!("host")), driver.clone());
    let source = empty("resource-source");
    let exposed = token("projection", 1);
    let saved = Rc::new(RefCell::new(None::<Resource<Wrapped>>));
    let (send, receive) = oneshot::channel::<()>();
    let receiver = Rc::new(RefCell::new(Some(receive)));
    let (slot, gate) = (saved.clone(), receiver.clone());
    host.serve(
        source.clone(),
        Rc::new(move |_, _, declaration| {
            let gate = gate.clone();
            *slot.borrow_mut() = Some(declaration.resource(
                "wrapped",
                ResourceDeclaration::new(move |_| {
                    let receive = gate.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(Wrapped(Rc::new(Nominal(Cell::new(5)))))
                    }
                    .boxed_local()
                }),
            )?);
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source_ref = host
        .hold(
            source,
            json!({}),
            GetOptions {
                principal: Some(json!("source")),
                ..Default::default()
            },
        )
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(source_ref.status(), RefStatus::Loading);
    let calls = Rc::new(Cell::new(0));
    let mapped = calls.clone();
    let resource = saved.borrow().clone().unwrap();
    host.serve(
        exposed.clone(),
        Rc::new(move |_, _, _| {
            let mapped = mapped.clone();
            Ok(
                Implementation::new().map_opaque("native", resource.clone(), move |value| {
                    mapped.set(mapped.get() + 1);
                    Ok(value.0.clone())
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(exposed, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(reference.status(), RefStatus::Live);
    let pending = reference.opaque::<Nominal>("native").err().unwrap();
    assert_eq!(
        (pending.code, pending.reason.as_str()),
        (ErrorCode::NotReady, "resource")
    );
    assert_eq!(calls.get(), 0);
    send.send(()).unwrap();
    driver.run_until(source_ref.ready()).unwrap();
    assert_eq!(reference.opaque::<Nominal>("native").unwrap().0.get(), 5);
    assert_eq!(calls.get(), 1);
    driver
        .run_until(host.invalidate_principal(|principal| principal == "source"))
        .unwrap();
    assert_eq!(reference.status(), RefStatus::Live);
    let closed = reference.opaque::<Nominal>("native").err().unwrap();
    assert_eq!(
        (closed.code, closed.reason.as_str()),
        (ErrorCode::NotReady, "resource")
    );
    assert_eq!(calls.get(), 1);
    driver.run_until(host.close()).unwrap();
}
