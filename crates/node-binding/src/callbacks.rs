use crate::session::Session;
use futures::future::LocalBoxFuture;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    rc::{Rc, Weak},
};
use tembra_core::{Result, RuntimeError};

pub(crate) struct Callback {
    pub session: Weak<Session>,
    pub generation: Option<Rc<crate::loader::Generation>>,
    pub id: String,
}
impl Callback {
    pub fn sync(&self, args: Vec<Value>) -> Result<Value> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        let _generation = session.enter_generation(self.generation.clone());
        session.dispatch(json!({"callback": self.id, "args": args}))
    }
    pub fn notify(&self, args: Vec<Value>) {
        if let Some(session) = self.session.upgrade() {
            let _generation = session.enter_generation(self.generation.clone());
            let result =
                session.dispatch(json!({"callback": self.id, "args": args, "discard": true}));
            if let Err(error) = result {
                let mut errors = session.callback_errors.borrow_mut();
                if errors.len() == 128 {
                    errors.remove(0);
                }
                errors.push(error);
            }
        }
    }
    pub fn sync_retained(&self, args: Vec<Value>) -> Result<Value> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        let _generation = session.enter_generation(self.generation.clone());
        session.dispatch(json!({"callback": self.id, "args": args, "retain": true}))
    }
    pub fn invoke_void(
        self: &Rc<Self>,
        args: Vec<Value>,
    ) -> LocalBoxFuture<'static, Result<Value>> {
        match self.session.upgrade() {
            Some(session) => session.invoke(self.clone(), args, false, true, 0),
            None => Box::pin(async { Err(RuntimeError::unavailable("closed", "Host ended")) }),
        }
    }
    pub fn invoke(
        self: &Rc<Self>,
        args: Vec<Value>,
        retain: bool,
    ) -> LocalBoxFuture<'static, Result<Value>> {
        match self.session.upgrade() {
            Some(session) => session.invoke(self.clone(), args, retain, false, 0),
            None => Box::pin(async { Err(RuntimeError::unavailable("closed", "Host ended")) }),
        }
    }
}
impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(generation) = &self.generation {
            generation.callbacks.set(generation.callbacks.get() - 1);
        }
        if let Some(session) = self.session.upgrade() {
            session
                .events
                .borrow_mut()
                .push(json!({"releaseCallback": self.id}));
        }
    }
}

/// Owns an application value slot without moving a Node handle or changing resource lifetime.
pub(crate) struct NodeValue {
    pub id: String,
    session: Weak<Session>,
    owned: Cell<bool>,
    generation: Option<Rc<crate::loader::Generation>>,
}
impl NodeValue {
    pub fn new(session: &Rc<Session>, value: &Value) -> Result<Self> {
        let id = value
            .get("$value")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::invalid("javascript", "missing retained value"))?;
        let generation = if let Some(id) = value.get("$generation").and_then(Value::as_str) {
            Some(
                session
                    .generations
                    .borrow()
                    .get(id)
                    .and_then(Weak::upgrade)
                    .ok_or_else(|| {
                        RuntimeError::invalid(
                            "foreign_generation",
                            "retained value generation ended",
                        )
                    })?,
            )
        } else {
            session.generation.borrow().clone()
        };
        if let Some(handoff) = session.handoffs.borrow_mut().last_mut() {
            handoff.values.push(id.to_owned());
        }
        if let Some(generation) = &generation {
            generation.values.set(generation.values.get() + 1);
        }
        Ok(Self {
            generation,
            id: id.into(),
            session: Rc::downgrade(session),
            owned: Cell::new(true),
        })
    }
    pub fn wire(&self) -> Value {
        json!({"$value": self.id})
    }
    pub fn transfer(self) -> Value {
        self.owned.set(false);
        json!({"$value": self.id, "$take": true})
    }
}
impl Drop for NodeValue {
    fn drop(&mut self) {
        if let Some(generation) = &self.generation {
            generation.values.set(generation.values.get() - 1);
        }
        if self.owned.get()
            && let Some(session) = self.session.upgrade()
        {
            session
                .events
                .borrow_mut()
                .push(json!({"releaseValue": self.id}));
        }
    }
}
