use super::*;
use crate::Scope;

#[test]
fn delivery_and_scope_changes_visit_only_affected_listeners() {
    let host = Host::new();
    let mut listeners = vec![];
    for _ in 0..1_000 {
        let scope = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id: scope,
            owner: Owner::Host,
            facet: 0,
            parents: vec![],
            dependencies: vec![],
            resources: vec![],
            state_version: None,
            migrate: false,
            active: true,
            closing: false,
        })
        .unwrap();
        let listener = host
            .listen(scope, None, "events".into(), 1, 0, 2, 4, None, false)
            .unwrap();
        host.start_scope_listeners(scope);
        listeners.push((scope, listener));
    }
    for _ in 0..20 {
        assert!(host.drain().is_empty());
    }
    assert_eq!(host.inner.borrow().delivery_visits, 0);
    let (scope, listener) = listeners[42];
    host.enqueue(
        listener,
        Delivery {
            payload: 10,
            bytes: 1,
        },
    )
    .unwrap();
    host.enqueue(
        listener,
        Delivery {
            payload: 11,
            bytes: 1,
        },
    )
    .unwrap();
    assert_eq!(
        host.drain(),
        [Action::Deliver {
            subscription: listener,
            payload: 10,
            operation: 2
        }]
    );
    assert_eq!(host.inner.borrow().delivery_visits, 1);
    host.stop_scope_listeners(scope, true).unwrap();
    host.finish_delivery(listener).unwrap();
    assert_eq!(host.drain(), [Action::DropValue(10)]);
    host.start_scope_listeners(scope);
    assert_eq!(
        host.drain(),
        [Action::Deliver {
            subscription: listener,
            payload: 11,
            operation: 2
        }]
    );
    host.stop_scope_listeners(scope, false).unwrap();
    assert!(host.inner.borrow().subscriptions.contains_key(&listener));
    host.finish_delivery(listener).unwrap();
    assert!(!host.inner.borrow().subscriptions.contains_key(&listener));
    assert_eq!(host.inner.borrow().subscriptions.len(), 999);
    assert_eq!(host.inner.borrow().delivery_visits, 2);
}

#[test]
fn retiring_a_previous_scope_preserves_successor_delivery_readiness() {
    let host = Host::new();
    let mut scopes = vec![];
    for _ in 0..2 {
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner: Owner::Host,
            facet: 0,
            parents: vec![],
            dependencies: vec![],
            resources: vec![],
            state_version: None,
            migrate: false,
            active: true,
            closing: false,
        })
        .unwrap();
        scopes.push(id);
    }
    let listener = host
        .listen(scopes[0], None, "events".into(), 1, 0, 2, 4, None, false)
        .unwrap();
    assert_eq!(
        host.listen(
            scopes[1],
            Some(scopes[0]),
            "events".into(),
            1,
            0,
            2,
            4,
            None,
            false
        )
        .unwrap(),
        listener
    );
    host.start_scope_listeners(scopes[1]);
    host.enqueue(
        listener,
        Delivery {
            payload: 10,
            bytes: 1,
        },
    )
    .unwrap();
    host.stop_scope_listeners(scopes[0], false).unwrap();
    assert_eq!(
        host.drain(),
        [Action::Deliver {
            subscription: listener,
            payload: 10,
            operation: 2
        }]
    );
}
