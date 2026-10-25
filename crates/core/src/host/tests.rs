use super::*;

fn install(host: &Host) -> InstalledFacet {
    let reload = host
        .prepare(vec![Facet::new("example", 1)], vec![])
        .unwrap();
    let facet = reload.facets[0].clone();
    host.commit(reload.id, vec![]).unwrap();
    facet
}

#[test]
fn admitted_roots_retain_old_versions_but_new_task_roots_use_current_code() {
    let host = Host::new();
    let old = install(&host);
    let root = host.begin_root("host").unwrap();
    let task = host.begin_task(Owner::Facet(old.owner)).unwrap();
    let current = install(&host);
    for _ in 0..100 {
        let root = host.begin_root("host").unwrap();
        host.end_root(root).unwrap();
    }
    assert_eq!(host.inner.borrow().versions.len(), 2);
    assert!(host.facet(old.id).is_ok());
    let retained = host.task_root(task, "host").unwrap();
    assert_eq!(
        host.inner.borrow().roots[&retained].version,
        current.code_version
    );
    host.end_root(root).unwrap();
    assert!(host.facet(old.id).is_err());
    host.finish_task(task).unwrap();
    host.end_root(retained).unwrap();
    assert_eq!(host.inner.borrow().versions.len(), 1);
    assert!(host.facet(old.id).is_err());
    assert!(host.facet(current.id).is_ok());
    assert!(host.inner.borrow().version_users.is_empty());
    assert!(host.drain().contains(&Action::RetireFacet(old.id)));
}

#[test]
fn an_unrelated_task_does_not_exhaust_the_retained_version_limit() {
    let host = Host::new();
    let owner = install(&host).owner;
    let task = host.begin_task(Owner::Facet(owner)).unwrap();
    for _ in 0..40 {
        let facet = install(&host);
        let root = host.task_root(task, "host").unwrap();
        assert_eq!(host.inner.borrow().roots[&root].version, facet.code_version);
        host.end_root(root).unwrap();
        assert_eq!(host.inner.borrow().versions.len(), 1);
    }
    host.finish_task(task).unwrap();
}

#[test]
fn shutdown_releases_only_the_dependents_of_each_object() {
    let host = Host::new();
    let mut facet = Facet::new("example", 1);
    facet.services.push(Service::new("example", ["read"]));
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let get = |key: &str| {
        host.acquire(
            Address {
                service: "example".into(),
                key: vec![key.into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap()
    };
    let consumer = get("consumer");
    let dependency = get("dependency");
    host.ready(consumer.object).unwrap();
    host.ready(dependency.object).unwrap();
    host.dependency(consumer.object, dependency.reference)
        .unwrap();
    host.close();
    assert_eq!(host.drain(), [Action::DisposeObject(consumer.object)]);
    host.disposed(consumer.object).unwrap();
    assert_eq!(host.drain(), [Action::DisposeObject(dependency.object)]);
    host.disposed(dependency.object).unwrap();
    host.finish_close().unwrap();
}

#[test]
fn a_forgotten_dependency_cannot_block_provider_shutdown() {
    let host = Host::new();
    let mut facet = Facet::new("example", 1);
    let mut service = Service::new("example", ["read"]);
    service.linger_ms = 100;
    facet.services.push(service);
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    let get = |key: &str| {
        host.acquire(
            Address {
                service: "example".into(),
                key: vec![key.into()],
                principal: "host".into(),
            },
            1,
            BTreeSet::new(),
        )
        .unwrap()
    };
    let consumer = get("consumer");
    let provider = get("provider");
    host.ready(consumer.object).unwrap();
    host.ready(provider.object).unwrap();
    host.dependency(consumer.object, provider.reference)
        .unwrap();
    let scope = host.allocate_handle().unwrap();
    host.register_scope(crate::Scope {
        id: scope,
        owner: Owner::Host,
        facet: 0,
        parents: vec![],
        dependencies: vec![provider.reference],
        resources: vec![],
        state_version: None,
        migrate: false,
        active: true,
        closing: false,
    })
    .unwrap();
    host.release(provider.reference).unwrap();
    host.forget_reference(provider.reference).unwrap();
    assert!(
        !host
            .inner
            .borrow()
            .object_dependents
            .contains(&provider.object)
    );
    assert!(
        !host
            .inner
            .borrow()
            .scope_dependencies
            .contains(&provider.object)
    );
    host.close();
    let actions = host.drain();
    assert!(actions.contains(&Action::DisposeObject(provider.object)));
    assert!(actions.contains(&Action::DisposeObject(consumer.object)));
}
