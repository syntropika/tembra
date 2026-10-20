use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

#[test]
fn migrated_reset_callback_can_enter_a_fresh_root_and_install_after_coherent_commit() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("publication-gate", json!(null)),
        driver.clone(),
    );
    let contract = Rc::new(
        Contract::local(
            "migrated",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "value".into(),
                    Member::State {
                        schema: json!({"type":"integer"}),
                    },
                ),
                (
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                ),
            ]),
        )
        .unwrap(),
    );
    let definition = |version| {
        let contract = contract.clone();
        ExecutableFacet::foreign("migrated-owner", vec![], move |_, setup| {
            setup.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let value = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(1))
                            .version(version)
                            .with_migrate(|old, _| Ok(json!(old.as_i64().unwrap() + 10))),
                    )?;
                    let read = value.clone();
                    Ok(Implementation::new()
                        .state("value", value.readable())
                        .json_method("read", move |_, _| {
                            let read = read.clone();
                            async move { Ok(Some(read.snapshot()?.value().clone())) }.boxed_local()
                        }))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap()
    };
    driver
        .run_until(
            host.install_executable(definition(1), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let reference = driver
        .run_until(host.get(contract.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let installs = Rc::new(RefCell::new(Vec::new()));
    let (observed_calls, observed_installs, reentrant, hosting) = (
        calls.clone(),
        installs.clone(),
        reference.clone(),
        host.clone(),
    );
    let subscription = reference
        .readable("value")
        .unwrap()
        .subscribe_events(move |event| {
            if matches!(event, StateEvent::Reset(_)) {
                observed_calls
                    .borrow_mut()
                    .push(reentrant.call_json("read", None, None).now_or_never());
                let token =
                    Rc::new(Contract::local("from-commit", 1, json!({}), BTreeMap::new()).unwrap());
                observed_installs.borrow_mut().push(hosting.serve(
                    token,
                    Rc::new(|_, _, _| Ok(Implementation::new())),
                    ServeOptions::default(),
                ));
            }
            Ok(())
        })
        .unwrap();
    let report = driver
        .run_until(host.reload(vec![definition(2)], ReloadOptions::default()))
        .unwrap();
    assert_eq!(report.current_version(), 2);
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(
        calls.borrow()[0].as_ref().unwrap().as_ref().unwrap(),
        &Some(json!(11))
    );
    assert!(
        installs.borrow()[0].is_ok(),
        "Ready must be released before user publication"
    );
    subscription.unsubscribe();
    reference.release();
    driver.run_until(host.close()).unwrap();
}
