//! Event-source adapters; queue bounds, listener ownership and task roots remain in core.
use crate::{callbacks::NodeValue, handles::Entry, session::Session};
use serde_json::{Value, json};
use std::rc::{Rc, Weak};
use tembra_core::{EventSource, Result, RuntimeError};

pub(crate) struct Delivery {
    pub listener: Rc<dyn Fn(Rc<NodeValue>)>,
}
pub(crate) struct EventBinding {
    value: NodeValue,
    session: Weak<Session>,
    pub source: EventSource<Rc<NodeValue>>,
}
impl EventBinding {
    fn subscribe(
        self: &Rc<Self>,
        listener: Rc<dyn Fn(Rc<NodeValue>)>,
    ) -> Result<Box<dyn FnOnce()>> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        let delivery = Rc::new(Delivery { listener });
        let sink = session.issue(Entry::Sink(Rc::downgrade(&delivery)))?;
        let sink_lease = SinkLease {
            session: Rc::downgrade(&session),
            id: crate::factory::handle(&sink)?.into(),
        };
        let output = session.dispatch(json!({"eventSource":self.value.id, "sink":sink}))?;
        let detach = session.callback(&output)?;
        let binding = self.clone();
        Ok(Box::new(move || {
            let _binding = binding;
            detach.notify(Vec::new());
            drop(delivery);
            drop(sink_lease);
        }))
    }
}
struct SinkLease {
    session: Weak<Session>,
    id: String,
}
impl Drop for SinkLease {
    fn drop(&mut self) {
        if let Some(session) = self.session.upgrade() {
            session.remove(&self.id);
        }
    }
}
pub(crate) fn event_source(session: &Rc<Session>, value: &Value) -> Result<Rc<EventBinding>> {
    let identity = value
        .get("$source")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::invalid("event_source", "expected source identity"))?;
    let retained = NodeValue::new(session, value)?;
    if let Some(binding) = session
        .event_sources
        .borrow()
        .get(identity)
        .and_then(Weak::upgrade)
    {
        return Ok(binding);
    }
    let value = retained;
    let binding = Rc::new_cyclic(|weak: &Weak<EventBinding>| {
        let captured = weak.clone();
        EventBinding {
            value,
            session: Rc::downgrade(session),
            source: EventSource::new(move |listener| {
                captured
                    .upgrade()
                    .ok_or_else(|| RuntimeError::invalid("event_source", "source ended"))?
                    .subscribe(listener)
            }),
        }
    });
    session
        .event_sources
        .borrow_mut()
        .retain(|_, entry| entry.strong_count() > 0);
    session
        .event_sources
        .borrow_mut()
        .insert(identity.into(), Rc::downgrade(&binding));
    Ok(binding)
}
