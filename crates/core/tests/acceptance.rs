use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use futures::FutureExt;
use serde_json::json;
use tembra_contracts::{Contract, Member, ValueSchema};
use tembra_core::{
    Context, GetOptions, Host, HostOptions, Implementation, ManualDriver, ServeOptions,
};

#[test]
fn identity_normalization_and_native_values_cross_real_reference_dispatch() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let contract = Rc::new(
            Contract::local(
                "acceptance.identity",
                1,
                json!({"type":"object", "properties":{
                    "name":{"type":"string"},
                    "mode":{"type":"string","default":"default"}
                }, "required":["name"]}),
                BTreeMap::from([
                    (
                        "token".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Native("acceptance.token@1".into())),
                        },
                    ),
                    (
                        "native".into(),
                        Member::Method {
                            args: Some(ValueSchema::Native("acceptance.native@1".into())),
                            result: Some(ValueSchema::Native("acceptance.native@1".into())),
                        },
                    ),
                ]),
            )
            .unwrap(),
        );
        let host = Host::new(
            HostOptions::new("acceptance", json!({"kind":"host"})),
            driver.clone(),
        );
        let activations = Rc::new(Cell::new(0));
        let spy = activations.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |input, _principal, _declarations| {
                spy.set(spy.get() + 1);
                assert_eq!(input["mode"], "default");
                assert!(input.get("ignored").is_none());
                let token = Rc::new(());
                Ok(Implementation::new()
                    .method::<(), Rc<()>>("token", move |(), _ctx| {
                        let token = token.clone();
                        async move { Ok(token) }.boxed_local()
                    })
                    .method::<Rc<Cell<u32>>, Rc<Cell<u32>>>("native", |native, _ctx| {
                        async move {
                            native.set(native.get() + 1);
                            Ok(native)
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let first = host
            .hold(
                contract.clone(),
                json!({"name":"same","ignored":42}),
                GetOptions::default(),
            )
            .unwrap();
        let second = host
            .hold(
                contract.clone(),
                json!({"mode":"default","name":"same"}),
                GetOptions::default(),
            )
            .unwrap();
        let (a, b) = futures::join!(first.ready(), second.ready());
        a.unwrap();
        b.unwrap();
        assert_eq!(activations.get(), 1);
        let first_token: Rc<()> = first.call("token", (), None).await.unwrap();
        let second_token: Rc<()> = second.call("token", (), None).await.unwrap();
        assert!(Rc::ptr_eq(&first_token, &second_token));

        let native = Rc::new(Cell::new(4_u32));
        let returned: Rc<Cell<u32>> = first.call("native", native.clone(), None).await.unwrap();
        assert!(Rc::ptr_eq(&native, &returned));
        assert_eq!(native.get(), 5);
        assert!(
            first
                .call::<String, Rc<Cell<u32>>>("native", "wrong native type".into(), None)
                .await
                .is_err()
        );
        first.release();
        assert!(first.call::<(), Rc<()>>("token", (), None).await.is_err());
        assert!(Rc::ptr_eq(
            &second_token,
            &second.call::<(), Rc<()>>("token", (), None).await.unwrap()
        ));
        second.release();
        host.close().await.unwrap();
    });
}

#[test]
fn nested_object_calls_preserve_root_and_switch_to_host_principal() {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let make_contract = |name| {
            Rc::new(
                Contract::local(
                    name,
                    1,
                    json!({"type":"object","properties":{}}),
                    BTreeMap::from([(
                        "inspect".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Native("acceptance.context@1".into())),
                        },
                    )]),
                )
                .unwrap(),
            )
        };
        let outer = make_contract("acceptance.outer");
        let inner = make_contract("acceptance.inner");
        let host = Host::new(HostOptions::new("contexts", json!("host")), driver.clone());
        let principals = Rc::new(RefCell::new(Vec::new()));
        let observed = principals.clone();
        host.serve(
            inner.clone(),
            Rc::new(move |_, principal, _| {
                observed.borrow_mut().push(("inner", principal));
                Ok(
                    Implementation::new().method::<(), Context>("inspect", |(), ctx| {
                        async move { Ok(ctx) }.boxed_local()
                    }),
                )
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let observed = principals.clone();
        host.serve(
            outer.clone(),
            Rc::new(move |_, principal, _| {
                observed.borrow_mut().push(("outer", principal));
                let inner = inner.clone();
                Ok(
                    Implementation::new().method::<(), Context>("inspect", move |(), ctx| {
                        let inner = inner.clone();
                        async move {
                            assert_eq!(ctx.principal(), &json!("host"));
                            let reference =
                                ctx.get(inner, json!({}), GetOptions::default()).await?;
                            let nested: Context = reference.call("inspect", (), Some(&ctx)).await?;
                            assert_eq!(nested.root_id(), ctx.root_id());
                            assert_eq!(nested.code_version(), ctx.code_version());
                            assert_eq!(nested.principal(), &json!("host"));
                            assert_eq!(nested.check(ctx.host_id()).unwrap_err().reason, "expired");
                            Ok(ctx)
                        }
                        .boxed_local()
                    }),
                )
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let escaped = host
            .root_as(json!("guest"), move |ctx| {
                async move {
                    let reference = ctx.get(outer, json!({}), GetOptions::default()).await?;
                    let child: Context = reference.call("inspect", (), Some(&ctx)).await?;
                    assert_eq!(child.root_id(), ctx.root_id());
                    assert_eq!(child.code_version(), ctx.code_version());
                    Ok(child)
                }
                .boxed_local()
            })
            .await
            .unwrap();
        assert_eq!(escaped.check(host.id()).unwrap_err().reason, "expired");
        assert_eq!(
            *principals.borrow(),
            [("outer", json!("guest")), ("inner", json!("host"))]
        );
        host.close().await.unwrap();
    });
}
