use crate::Reason;
use crate::{
    Action, Code, Error, Host, Id, Owner, Result,
    host::{conflict, gone},
};
use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Delivery {
    pub payload: Id,
    pub bytes: usize,
}

/// A bounded connection. Payload identities refer to adapter-owned values.
#[derive(Clone, Debug)]
pub struct Connection {
    pub id: Id,
    pub owner: Owner,
    pub source: Id,
    pub operation: Id,
    pub max_pending: usize,
    pub max_bytes: Option<usize>,
    pub coalesce: bool,
    pub(crate) queue: VecDeque<Delivery>,
    pub(crate) bytes: usize,
    pub(crate) running: Option<Id>,
    pub(crate) stopped: bool,
    name: String,
    sizing: Id,
    scopes: BTreeSet<Id>,
    current: Option<Id>,
    active: bool,
}

impl Host {
    pub fn subscribe(
        &self,
        owner: Owner,
        source: Id,
        operation: Id,
        max_pending: usize,
        max_bytes: Option<usize>,
        coalesce: bool,
    ) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        inner.owner(owner)?;
        if source == 0 || operation == 0 || max_pending == 0 || max_bytes == Some(0) {
            return Err(Error::invalid(
                Reason::Subscription,
                "positive identities and queue limits are required",
            ));
        }
        let id = inner.id()?;
        inner.owner_listeners.insert(owner, id);
        inner.subscriptions.insert(
            id,
            Connection {
                id,
                owner,
                source,
                operation,
                max_pending,
                max_bytes,
                coalesce,
                queue: VecDeque::new(),
                bytes: 0,
                running: None,
                stopped: false,
                name: String::new(),
                sizing: 0,
                scopes: BTreeSet::new(),
                current: None,
                active: false,
            },
        );
        Ok(id)
    }

    /// Reuses a connection only when its source identity and queue contract are unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn listen(
        &self,
        scope: Id,
        previous: Option<Id>,
        name: String,
        source: Id,
        sizing: Id,
        operation: Id,
        max_pending: usize,
        max_bytes: Option<usize>,
        coalesce: bool,
    ) -> Result<Id> {
        let owner = self
            .inner
            .borrow()
            .scopes
            .get(&scope)
            .ok_or_else(|| gone(Reason::Scope))?
            .owner;
        {
            let mut inner = self.inner.borrow_mut();
            let candidates: BTreeSet<_> = inner
                .scope_listeners
                .members(&scope)
                .chain(
                    previous
                        .into_iter()
                        .flat_map(|scope| inner.scope_listeners.members(&scope)),
                )
                .collect();
            if let Some(id) = candidates.into_iter().find(|id| {
                let connection = &inner.subscriptions[id];
                !connection.stopped
                    && connection.name == name
                    && connection.source == source
                    && connection.sizing == sizing
                    && connection.max_pending == max_pending
                    && connection.max_bytes == max_bytes
                    && connection.coalesce == coalesce
            }) {
                inner
                    .subscriptions
                    .get_mut(&id)
                    .expect("indexed listener")
                    .scopes
                    .insert(scope);
                inner.scope_listeners.insert(scope, id);
                return Ok(id);
            }
        }
        let id = self.subscribe(owner, source, operation, max_pending, max_bytes, coalesce)?;
        let mut inner = self.inner.borrow_mut();
        let connection = inner
            .subscriptions
            .get_mut(&id)
            .expect("created connection");
        connection.name = name;
        connection.sizing = sizing;
        connection.scopes.insert(scope);
        inner.scope_listeners.insert(scope, id);
        Ok(id)
    }

    pub(crate) fn stop_scope_listeners(&self, scope: Id, keep: bool) -> Result<()> {
        let detached = {
            let mut inner = self.inner.borrow_mut();
            let mut detached = Vec::new();
            let ids: Vec<_> = inner.scope_listeners.members(&scope).collect();
            for id in ids {
                let connection = inner.subscriptions.get_mut(&id).expect("indexed listener");
                if connection.current == Some(scope) {
                    connection.active = false;
                }
                if !keep {
                    connection.scopes.remove(&scope);
                    if connection.scopes.is_empty() {
                        detached.push(connection.id);
                    }
                }
                let ready = connection.active
                    && !connection.stopped
                    && connection.running.is_none()
                    && !connection.queue.is_empty();
                if !keep {
                    inner.scope_listeners.remove(&scope, id);
                }
                if ready {
                    inner.ready_deliveries.insert(id);
                } else {
                    inner.ready_deliveries.remove(&id);
                }
            }
            detached
        };
        for id in detached {
            self.unsubscribe(id)?;
            self.inner
                .borrow_mut()
                .actions
                .push_back(Action::DetachSubscription(id));
        }
        Ok(())
    }

    pub(crate) fn start_scope_listeners(&self, scope: Id) {
        let mut inner = self.inner.borrow_mut();
        let ids: Vec<_> = inner.scope_listeners.members(&scope).collect();
        for id in ids {
            let connection = inner.subscriptions.get_mut(&id).expect("indexed listener");
            if !connection.stopped {
                connection.current = Some(scope);
                connection.active = true;
                if connection.running.is_none() && !connection.queue.is_empty() {
                    inner.ready_deliveries.insert(id);
                }
            }
        }
    }

    pub(crate) fn pump_deliveries(&self) {
        let ready = std::mem::take(&mut self.inner.borrow_mut().ready_deliveries);
        for subscription in ready {
            #[cfg(test)]
            {
                self.inner.borrow_mut().delivery_visits += 1;
            }
            let operation = {
                let inner = self.inner.borrow();
                let Some(connection) = inner.subscriptions.get(&subscription) else {
                    continue;
                };
                if !connection.active || connection.stopped || connection.running.is_some() {
                    continue;
                }
                connection.operation
            };
            if let Ok(Some(delivery)) = self.next_delivery(subscription) {
                self.inner.borrow_mut().actions.push_back(Action::Deliver {
                    subscription,
                    operation,
                    payload: delivery.payload,
                });
            }
        }
    }

    /// Takes ownership of a payload on success. Failure leaves the submitted payload adapter-owned.
    pub fn enqueue(&self, subscription: Id, delivery: Delivery) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let connection = inner
            .subscriptions
            .get_mut(&subscription)
            .ok_or_else(|| gone(Reason::Subscription))?;
        if connection.stopped {
            return Err(gone(Reason::Subscription));
        }
        if delivery.payload == 0 {
            return Err(Error::invalid(
                Reason::Payload,
                "payload identity is required",
            ));
        }
        let mut dropped = Vec::new();
        if connection.coalesce {
            dropped.extend(connection.queue.drain(..).map(|d| d.payload));
            connection.bytes = 0;
        }
        let overflow = connection.queue.len() >= connection.max_pending
            || connection
                .max_bytes
                .is_some_and(|max| delivery.bytes > max.saturating_sub(connection.bytes));
        if overflow {
            connection.stopped = true;
            dropped.extend(connection.queue.drain(..).map(|d| d.payload));
            connection.bytes = 0;
        } else {
            connection.bytes += delivery.bytes;
            connection.queue.push_back(delivery);
        }
        if connection.active && connection.running.is_none() && !connection.stopped {
            inner.ready_deliveries.insert(subscription);
        } else {
            inner.ready_deliveries.remove(&subscription);
        }
        for payload in dropped {
            inner.actions.push_back(Action::DropValue(payload));
        }
        if overflow {
            inner
                .actions
                .push_back(Action::SubscriptionFailed(subscription));
            return Err(Error::new(
                Code::Exhausted,
                Reason::SubscriptionQueue,
                "subscription queue exceeded its limit",
            ));
        }
        Ok(())
    }

    pub fn next_delivery(&self, subscription: Id) -> Result<Option<Delivery>> {
        let mut inner = self.inner.borrow_mut();
        let connection = inner
            .subscriptions
            .get_mut(&subscription)
            .ok_or_else(|| gone(Reason::Subscription))?;
        if connection.running.is_some() {
            return Err(conflict(Reason::SubscriptionBusy));
        }
        if connection.stopped {
            return Ok(None);
        }
        let delivery = connection.queue.pop_front();
        if let Some(delivery) = delivery {
            connection.bytes -= delivery.bytes;
            connection.running = Some(delivery.payload);
        }
        Ok(delivery)
    }

    pub fn finish_delivery(&self, subscription: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let connection = inner
            .subscriptions
            .get_mut(&subscription)
            .ok_or_else(|| gone(Reason::Subscription))?;
        let payload = connection
            .running
            .take()
            .ok_or_else(|| conflict(Reason::SubscriptionIdle))?;
        let remove = connection.stopped;
        inner.actions.push_back(Action::DropValue(payload));
        if remove {
            inner.remove_subscription(subscription);
        } else if inner.subscriptions[&subscription].active
            && !inner.subscriptions[&subscription].queue.is_empty()
        {
            inner.ready_deliveries.insert(subscription);
        }
        Ok(())
    }

    pub fn unsubscribe(&self, subscription: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let Some(connection) = inner.subscriptions.get_mut(&subscription) else {
            return Ok(());
        };
        connection.stopped = true;
        let dropped: Vec<_> = connection.queue.drain(..).map(|d| d.payload).collect();
        connection.bytes = 0;
        let remove = connection.running.is_none();
        for payload in dropped {
            inner.actions.push_back(Action::DropValue(payload));
        }
        inner.ready_deliveries.remove(&subscription);
        if remove {
            inner.remove_subscription(subscription);
        }
        Ok(())
    }
}

impl crate::host::Inner {
    pub(crate) fn remove_subscription(&mut self, id: Id) {
        if let Some(connection) = self.subscriptions.remove(&id) {
            self.owner_listeners.remove(&connection.owner, id);
            for scope in connection.scopes {
                self.scope_listeners.remove(&scope, id);
            }
            self.ready_deliveries.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
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
}
