//! Native reactive snapshots. References bound their observers; the kernel publishes revisions.
use crate::{Error, Host, Result, core};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

type Read<T> = Rc<dyn Fn() -> Result<Rc<T>>>;
type Check = Rc<dyn Fn() -> Result<()>>;

pub(crate) struct Observer {
    pub active: Cell<bool>,
    pub invoke: Box<dyn Fn() -> Result<()>>,
}

#[derive(Default)]
pub(crate) struct ReadLifetime {
    pub released: Cell<bool>,
    pub observers: RefCell<Vec<Weak<Observer>>>,
}
impl ReadLifetime {
    pub fn close(&self) {
        self.released.set(true);
        for observer in self
            .observers
            .take()
            .into_iter()
            .filter_map(|observer| observer.upgrade())
        {
            observer.active.set(false);
        }
    }
}

/// A detachable native observation. Dropping it stops future notifications.
pub struct Observation(Rc<Observer>);
impl Observation {
    /// Stop receiving notifications; repeated detachment has no effect.
    pub fn detach(&self) {
        self.0.active.set(false);
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.detach();
    }
}

/// A checked immutable state view or cached selection.
pub struct Readable<T> {
    host: Host,
    object: core::Id,
    read: Read<T>,
    check: Check,
    lifetime: Option<Rc<ReadLifetime>>,
}
impl<T> Clone for Readable<T> {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            object: self.object,
            read: self.read.clone(),
            check: self.check.clone(),
            lifetime: self.lifetime.clone(),
        }
    }
}

impl<T: 'static> Readable<T> {
    pub(crate) fn new(host: Host, object: core::Id, lifetime: Option<Rc<ReadLifetime>>) -> Self {
        let reader = host.clone();
        let checked = host.clone();
        let claim = lifetime.clone();
        Self {
            host,
            object,
            lifetime,
            read: Rc::new(move || reader.value(reader.kernel().snapshot(object)?.value)),
            check: Rc::new(move || {
                if claim.as_ref().is_some_and(|claim| claim.released.get()) {
                    return Err(Error::new(
                        core::Code::Released,
                        core::Reason::Reference,
                        "reference is released",
                    ));
                }
                checked.kernel().snapshot(object).map(|_| ())
            }),
        }
    }

    /// Read the current snapshot while its owner and consuming reference remain valid.
    pub fn value(&self) -> Result<Rc<T>> {
        (self.check)()?;
        (self.read)()
    }

    /// Compute a cached selection. Equal results retain their identity and do not notify observers.
    pub fn select<U: PartialEq + 'static>(
        &self,
        select: impl Fn(&T) -> Result<U> + 'static,
    ) -> Readable<U> {
        let source = self.clone();
        let cache = RefCell::new(None::<(Rc<T>, Rc<U>)>);
        Readable {
            host: self.host.clone(),
            object: self.object,
            check: self.check.clone(),
            lifetime: self.lifetime.clone(),
            read: Rc::new(move || {
                let value = source.value()?;
                if let Some((previous, selected)) = &*cache.borrow()
                    && Rc::ptr_eq(previous, &value)
                {
                    return Ok(selected.clone());
                }
                let next = select(&value)?;
                let retained = cache
                    .borrow()
                    .as_ref()
                    .filter(|(_, selected)| **selected == next)
                    .map(|(_, selected)| selected.clone())
                    .unwrap_or_else(|| Rc::new(next));
                *cache.borrow_mut() = Some((value, retained.clone()));
                Ok(retained)
            }),
        }
    }

    /// Deliver the current value immediately and future changes after adoption.
    /// Observer failures are reported by the host and never roll back adopted state.
    pub fn subscribe(&self, handle: impl Fn(Rc<T>) -> Result<()> + 'static) -> Result<Observation> {
        let value = self.value()?;
        handle(value.clone())?;
        let previous = RefCell::new(value);
        let source = self.clone();
        let observer = Rc::new(Observer {
            active: Cell::new(true),
            invoke: Box::new(move || {
                let value = source.value()?;
                if Rc::ptr_eq(&previous.borrow(), &value) {
                    return Ok(());
                }
                *previous.borrow_mut() = value.clone();
                handle(value)
            }),
        });
        self.host.observe(self.object, Rc::downgrade(&observer));
        if let Some(lifetime) = &self.lifetime {
            lifetime
                .observers
                .borrow_mut()
                .push(Rc::downgrade(&observer));
        }
        Ok(Observation(observer))
    }
}
