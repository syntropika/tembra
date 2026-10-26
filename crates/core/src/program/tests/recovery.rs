use super::*;

#[test]
fn restored_dependencies_and_memberships_use_the_new_identity() {
    let host = Host::new();
    let base = host
        .resource(Owner::Host, "base".into(), 1, false, vec![])
        .unwrap();
    let dependent = host
        .resource(Owner::Host, "dependent".into(), 1, false, vec![base.id])
        .unwrap();
    let scope = host_scope(&host, vec![base.id, dependent.id], vec![]);
    assert!(host.close_resource(dependent.id).unwrap());
    host.resource_closed(dependent.id).unwrap();
    assert!(host.close_resource(base.id).unwrap());
    host.resource_closed(base.id).unwrap();
    let next = host
        .resource(Owner::Host, "base".into(), 1, false, vec![])
        .unwrap();
    host.replace_resource(base.id, next.id);
    let restored = host
        .resource(Owner::Host, "dependent".into(), 1, false, vec![next.id])
        .unwrap();
    host.replace_resource(dependent.id, restored.id);
    assert_eq!(
        host.inner.borrow().scopes[&scope].resources,
        [next.id, restored.id]
    );
    assert!(!host.close_resource(next.id).unwrap());
    assert!(host.close_resource(restored.id).unwrap());
    host.resource_closed(restored.id).unwrap();
    assert!(host.close_resource(next.id).unwrap());
}
