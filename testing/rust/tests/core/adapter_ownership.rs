//! Terminal reference wrappers must not retain withdrawn service adapter captures.
use futures::FutureExt;
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

fn contract(version: u64, member: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "adapter.ownership",
            version,
            json!({}),
            BTreeMap::from([(
                member.into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                },
            )]),
        )
        .unwrap(),
    )
}

#[test]
fn terminal_reference_does_not_retain_withdrawn_service_adapter_capture() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("adapters", json!(null)), driver.clone());
    let owner = Rc::new(String::from("adapter generation"));
    let weak = Rc::downgrade(&owner);
    host.serve(
        contract(2, "current"),
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .json_method("current", |_, _| async { Ok(Some(json!(7))) }.boxed_local()))
        }),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |source| {
                let _capture = &owner;
                Implementation::new().project_method("legacy", source, "current")
            }),
        ),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract(1, "legacy"), json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(
        driver
            .run_until(reference.call_json("legacy", None, None))
            .unwrap(),
        Some(json!(7))
    );
    assert!(
        weak.upgrade().is_some(),
        "an installed service still owns its adapter"
    );
    driver.run_until(host.close()).unwrap();
    drop(host);
    assert_eq!(reference.status(), RefStatus::Gone);
    assert!(
        weak.upgrade().is_none(),
        "terminal wrappers must not pin service callbacks"
    );
    assert!(
        driver
            .run_until(reference.call_json("legacy", None, None))
            .is_err()
    );
}
