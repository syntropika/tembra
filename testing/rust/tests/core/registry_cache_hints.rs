use futures::FutureExt;
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

#[test]
fn target_hints_compare_real_weak_identity_without_granting_authority_or_keeping_owner_alive() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("cache", Value::Null), driver.clone());
    let registry = Registry::new("cache", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let token = Rc::new(
        Contract::local(
            "cache",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "a".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                ),
                (
                    "b".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                ),
            ]),
        )
        .unwrap(),
    );
    let capture = Rc::new(Cell::new(0));
    let weak = Rc::downgrade(&capture);
    host.serve(
        token,
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "alias", Value::Null, "a")?;
            declaration.register_method(&registry, "a", Value::Null, "a")?;
            declaration.register_method(&registry, "b", Value::Null, "b")?;
            Ok(Implementation::new()
                .retain_owner(capture.clone())
                .method("a", |(): (), _| async { Ok(()) }.boxed_local())
                .method("b", |(): (), _| async { Ok(()) }.boxed_local()))
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let table = view.table(None).unwrap();
    let a = table.get("a").unwrap().operation().unwrap().clone();
    let alias = table.get("alias").unwrap().operation().unwrap().clone();
    let b = table.get("b").unwrap().operation().unwrap().clone();
    assert!(a.same_target(&alias));
    assert!(!a.same_target(&b));
    assert!(a.is_live_target());
    let saved = Rc::new(std::cell::RefCell::new(None));
    let sink = saved.clone();
    driver
        .run_until(host.root(move |context| {
            *sink.borrow_mut() = Some(context);
            async { Ok(()) }.boxed_local()
        }))
        .unwrap();
    assert!(
        a.is_live_target(),
        "liveness is not an expired Context authority check"
    );
    assert_eq!(
        driver
            .run_until(a.call::<(), ()>(&saved.borrow_mut().take().unwrap(), ()))
            .unwrap_err()
            .reason,
        "expired"
    );
    driver.run_until(host.close()).unwrap();
    assert!(!a.is_live_target());
    assert!(
        a.same_target(&alias),
        "retirement does not rewrite identity"
    );
    assert!(
        weak.upgrade().is_none(),
        "cached operations/snapshots keep no owner capture"
    );
}
