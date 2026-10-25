use std::collections::BTreeSet;
use tembra_core::*;

fn provider(name: &str, serves: bool) -> Facet {
    let mut facet = Facet::new(name, 1);
    if serves {
        facet.services.push(Service::new("item", ["read"]));
    }
    facet
}

#[test]
fn provider_move_keeps_identity_snapshot_old_roots_and_affected_index() {
    let host = Host::new();
    let initial = host
        .prepare(vec![provider("a", true), provider("b", false)], vec![])
        .unwrap();
    host.commit(initial.id, vec![]).unwrap();
    let address = Address {
        service: "item".into(),
        key: vec!["main".into()],
        principal: "host".into(),
    };
    let acquired = host
        .acquire(address.clone(), 1, BTreeSet::from(["read".into()]))
        .unwrap();
    let snapshot = host.initialize_state(acquired.object, 77, 1).unwrap();
    host.ready(acquired.object).unwrap();
    let old_root = host.begin_root("host").unwrap();
    let previous = host
        .begin_call(acquired.reference, old_root, "read")
        .unwrap();
    host.finish_call(previous.id).unwrap();
    let prepared = host
        .prepare(vec![provider("b", true)], vec!["a".into()])
        .unwrap();
    let destination = prepared.facets[0].id;
    assert_eq!(prepared.objects, vec![acquired.object]);
    assert_eq!(prepared.providers.get("item"), Some(&destination));
    assert!(host.facet_named("a").is_ok());
    assert_eq!(
        host.prepared_facet(prepared.id, "a").unwrap_err().code,
        Code::Gone
    );
    assert_eq!(
        host.prepared_facet(prepared.id, "b").unwrap().id,
        destination
    );
    host.commit(prepared.id, vec![]).unwrap();
    assert_eq!(host.status(acquired.reference).unwrap(), Status::Live);
    assert_eq!(host.snapshot(acquired.object).unwrap(), snapshot);
    let same = host.acquire(address, 1, BTreeSet::new()).unwrap();
    assert_eq!(same.object, acquired.object);
    assert_eq!(same.facet, destination);
    let current_root = host.begin_root("host").unwrap();
    let current = host
        .begin_call(acquired.reference, current_root, "read")
        .unwrap();
    assert_eq!(current.facet, destination);
    host.finish_call(current.id).unwrap();
    host.end_root(current_root).unwrap();
    let previous = host
        .begin_call(acquired.reference, old_root, "read")
        .unwrap();
    assert_eq!(previous.facet, acquired.facet);
    host.finish_call(previous.id).unwrap();
    host.end_root(old_root).unwrap();
    assert_eq!(
        host.prepared_facet(prepared.id, "b").unwrap_err().reason,
        Reason::ReloadTicket.as_str()
    );
    let next = host.prepare(vec![provider("b", true)], vec![]).unwrap();
    assert_eq!(next.objects, vec![acquired.object]);
    host.abort(next.id).unwrap();
}

#[test]
fn aborted_provider_move_preserves_installed_resolution() {
    let host = Host::new();
    let initial = host.prepare(vec![provider("a", true)], vec![]).unwrap();
    host.commit(initial.id, vec![]).unwrap();
    let original = host.facet_named("a").unwrap();
    let candidate = host
        .prepare(vec![provider("a", false), provider("b", true)], vec![])
        .unwrap();
    assert_eq!(host.facet_named("a").unwrap(), original);
    assert!(host.facet_named("b").is_err());
    assert!(host.prepared_facet(candidate.id, "b").is_ok());
    assert_eq!(
        host.prepared_facet(candidate.id + 1, "b")
            .unwrap_err()
            .reason,
        Reason::ReloadTicket.as_str()
    );
    host.abort(candidate.id).unwrap();
    assert_eq!(host.facet_named("a").unwrap(), original);
    assert!(host.facet_named("b").is_err());
    assert!(host.prepared_facet(candidate.id, "b").is_err());
}
