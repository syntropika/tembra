use super::*;
use crate::{Host, HostOptions, Implementation, Lifetime, ManualDriver, ServeOptions};
use serde_json::json;
use tembra_contracts::Contract;

fn fixture() -> (Host, Rc<ManualDriver>, Registry, Rc<ObjectRecord>) {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("private-registry-fixture", json!("host")),
        driver.clone(),
    );
    let registry = Registry::new("ordered", ConflictPolicy::Error).unwrap();
    let registration = registry.clone();
    let contract =
        Rc::new(Contract::local("registry.private", 1, json!({}), BTreeMap::new()).unwrap());
    host.serve(
        contract,
        Rc::new(move |_, _, declaration| {
            declaration.register(&registration, "first", json!(1))?;
            declaration.register(&registration, "second", json!(2))?;
            Ok(Implementation::new())
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let owner = host.inner.objects.borrow().values().next().unwrap().clone();
    (host, driver, registry, owner)
}

#[test]
fn private_remove_and_readd_retain_ordinals_and_stale_preflight_has_no_effect() {
    let (host, driver, registry, owner) = fixture();
    let store = &host.inner.registries;
    let view = host.registry(&registry).unwrap();
    let before = view.table(None).unwrap();
    let mut removed = store.remove_owner(owner.id).unwrap();
    let stale = store.remove_owner(owner.id).unwrap();
    store.preflight(&removed).unwrap();
    let publications = store.adopt(&mut removed);
    host.inner
        .publication_queue
        .append_registry_batch(publications);
    removed.finish();
    assert!(view.table(None).unwrap().is_empty());
    assert_eq!(
        store.preflight(&stale).unwrap_err().reason,
        "registry_candidate"
    );
    assert!(view.table(None).unwrap().is_empty());
    let mut declarations = owner.registrations.borrow().clone();
    declarations.reverse();
    let mut restored = store.prepare_activation(&owner, &declarations, 1).unwrap();
    store.preflight(&restored).unwrap();
    let publications = store.adopt(&mut restored);
    host.inner
        .publication_queue
        .append_registry_batch(publications);
    restored.finish();
    assert_eq!(
        view.table(None)
            .unwrap()
            .entries()
            .map(|(key, _)| key.to_owned())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(before.len(), 2);
    // Empty declarations must still remove an existing owner's contributions.
    let mut replacement = store.prepare_activation(&owner, &[], 1).unwrap();
    store.preflight(&replacement).unwrap();
    let publications = store.adopt(&mut replacement);
    host.inner
        .publication_queue
        .append_registry_batch(publications);
    replacement.finish();
    assert!(view.table(None).unwrap().is_empty());
    assert_eq!(before.len(), 2);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn exact_private_epoch_lookup_never_substitutes_current_or_retired_tables() {
    let (host, driver, registry, _owner) = fixture();
    let store = &host.inner.registries;
    let view = host.registry(&registry).unwrap();
    let epoch = Rc::new(Epoch {
        version: crate::epoch::CodeEpoch::new(2).unwrap(),
        cancellation: crate::CancellationToken::new(),
        admitted: Cell::new(true),
    });
    store.installed.borrow_mut().epochs.insert(
        2,
        EpochTables {
            epoch: epoch.clone(),
            owners: BTreeMap::new(),
            tables: BTreeMap::from([(
                registry.name.clone(),
                Rc::new(Table::new(vec![(
                    "epoch-two".into(),
                    RegistryEntry {
                        data: json!(2),
                        owner_id: 0,
                        operation: None,
                    },
                )])),
            )]),
        },
    );
    let (second, _scope) = Context::new_root(host.inner.id, 99, 2, json!("test"));
    let selected = view.table(Some(&second)).unwrap();
    assert_eq!(selected.code_version(), 2);
    assert_eq!(selected.get("epoch-two").unwrap().data(), &json!(2));
    assert_eq!(view.table(None).unwrap().code_version(), 1);
    assert!(view.table(None).unwrap().get("epoch-two").is_none());
    epoch.admitted.set(false);
    assert_eq!(
        view.table(Some(&second)).err().unwrap().code,
        ErrorCode::Superseded
    );
    assert_eq!(selected.get("epoch-two").unwrap().data(), &json!(2));
    let (missing, _scope) = Context::new_root(host.inner.id, 100, 3, json!("test"));
    assert_eq!(
        view.table(Some(&missing)).err().unwrap().reason,
        "code_version"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn a_refused_candidate_does_not_consume_key_ordinals() {
    let (host, driver, registry, _first) = fixture();
    host.serve(
        Rc::new(Contract::local("registry.other", 1, json!({}), BTreeMap::new()).unwrap()),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let owner = host
        .inner
        .objects
        .borrow()
        .values()
        .find(|object| object.contract.name() == "registry.other")
        .unwrap()
        .clone();
    let store = &host.inner.registries;
    let before = store.installed.borrow().order.clone();
    let declarations = vec![
        RegistrationDeclaration {
            registry: registry.clone(),
            key: "not-adopted".into(),
            data: json!(3),
            method: None,
        },
        RegistrationDeclaration {
            registry: registry.clone(),
            key: "first".into(),
            data: json!(4),
            method: None,
        },
    ];
    assert_eq!(
        store
            .prepare_activation(&owner, &declarations, 1)
            .err()
            .unwrap()
            .reason,
        "registry_conflict"
    );
    assert_eq!(store.installed.borrow().order, before);
    assert!(
        host.registry(&registry)
            .unwrap()
            .table(None)
            .unwrap()
            .get("not-adopted")
            .is_none()
    );
    driver.run_until(host.close()).unwrap();
}
