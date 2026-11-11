//! Attach sources and acknowledge deliveries only after native handlers settle.
use super::*;
use crate::events::{Detach, EventOptions, EventSource};

type Handler = Rc<dyn Fn(Context, Data) -> LocalBoxFuture<'static, Result<()>>>;
pub(super) type ListenerDefinition =
    Box<dyn FnOnce(Host, core::Id, core::Id, core::Owner) -> Result<()>>;

pub(super) struct Listener {
    orb: core::Id,
    owner: core::Owner,
    stopped: Cell<bool>,
    detach: RefCell<Option<Detach>>,
    handle: Handler,
}

impl Declaration {
    /// Attach a source during preparation and deliver typed values after activation.
    pub fn on<T: 'static, F, Fut>(
        &self,
        name: impl Into<String>,
        source: EventSource<T>,
        options: EventOptions<T>,
        handle: F,
    ) -> Result<()>
    where
        F: Fn(Context, Rc<T>) -> Fut + 'static,
        Fut: Future<Output = Result<()>> + 'static,
    {
        self.check_declaration()?;
        if self.orb == 0 {
            return Err(Error::invalid(
                Reason::Subscription,
                "host listeners require an orb owner",
            ));
        }
        let name = name.into();
        if name.is_empty() || !self.draft.listener_names.borrow_mut().insert(name.clone()) {
            return Err(Error::invalid(Reason::Duplicate, name));
        }
        let handler: Handler = Rc::new(move |ctx, value| {
            let value = value.downcast::<T>().map_err(|_| type_error());
            let result = value.map(|value| handle(ctx, value));
            async move { result?.await }.boxed_local()
        });
        self.draft
            .listeners
            .borrow_mut()
            .push(Box::new(move |host, scope, orb, owner| {
                let operation = host.kernel().allocate_handle()?;
                let subscription = host.kernel().listen(
                    scope,
                    None,
                    name,
                    host.kernel().allocate_handle()?,
                    if options.size.is_some() {
                        host.kernel().allocate_handle()?
                    } else {
                        0
                    },
                    operation,
                    options.max_pending,
                    options.max_bytes,
                    options.coalesce,
                )?;
                let record = Rc::new(Listener {
                    orb,
                    owner,
                    stopped: Cell::new(false),
                    detach: RefCell::new(None),
                    handle: handler,
                });
                host.0
                    .listeners
                    .borrow_mut()
                    .insert(subscription, record.clone());
                let captured = record.clone();
                let retained = host.clone();
                let attached = (source.0)(Rc::new(move |value| {
                    if captured.stopped.get() {
                        return;
                    }
                    let result = (|| {
                        let bytes = options
                            .size
                            .as_ref()
                            .map(|size| size(&value))
                            .transpose()?
                            .unwrap_or(0);
                        let payload = retained.kernel().allocate_handle()?;
                        retained.0.values.borrow_mut().insert(
                            payload,
                            ValueRecord {
                                value,
                                portable: None,
                            },
                        );
                        if let Err(error) = retained
                            .kernel()
                            .enqueue(subscription, core::Delivery { payload, bytes })
                        {
                            retained.drop_value(payload);
                            return Err(error);
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        retained.0.errors.borrow_mut().push(error);
                        retained.detach_listener(subscription);
                    }
                    retained.flush();
                }));
                match attached {
                    Ok(detach) => {
                        if record.stopped.get() {
                            detach();
                        } else {
                            *record.detach.borrow_mut() = Some(detach);
                        }
                    }
                    Err(error) => {
                        host.detach_listener(subscription);
                        return Err(error);
                    }
                }
                Ok(())
            }));
        Ok(())
    }
}

impl Host {
    pub(super) fn prepare_listeners(&self, scope: core::Id) -> Result<()> {
        let definitions = {
            let scopes = self.0.scopes.borrow();
            let Some(record) = scopes.get(&scope) else {
                return Ok(());
            };
            (record.orb, record.owner, record.draft.listeners.take())
        };
        for definition in definitions.2 {
            definition(self.clone(), scope, definitions.0, definitions.1)?;
        }
        Ok(())
    }

    /// Stop a native source once after a kernel detach or queue-failure action.
    pub fn detach_listener(&self, subscription: core::Id) -> bool {
        let record = self.0.listeners.borrow_mut().remove(&subscription);
        let Some(record) = record else {
            return false;
        };
        record.stopped.set(true);
        if let Some(detach) = record.detach.borrow_mut().take() {
            detach();
        }
        if let Err(error) = self.kernel().unsubscribe(subscription) {
            self.0.errors.borrow_mut().push(error);
        }
        true
    }

    /// Dispatch a native delivery selected by the shared kernel.
    pub fn deliver(&self, subscription: core::Id, operation: core::Id, payload: core::Id) -> bool {
        let record = self.0.listeners.borrow().get(&subscription).cloned();
        let Some(record) = record else {
            return false;
        };
        let admitted = (|| {
            let object = match record.owner {
                core::Owner::Object(object) => Some(object),
                _ => None,
            };
            let principal = object
                .map(|object| {
                    self.kernel()
                        .object_address(object)
                        .map(|address| address.principal)
                })
                .transpose()?
                .unwrap_or_else(|| "host".into());
            let root = self.kernel().begin_root(principal)?;
            let root_guard = super::context::RootGuard {
                host: self.clone(),
                root,
            };
            let call = self
                .kernel()
                .begin_operation(record.orb, root, operation, object)?;
            let entered = self.context(root, Some(call.clone()))?;
            let value = self
                .0
                .values
                .borrow()
                .get(&payload)
                .map(|record| record.value.clone())
                .ok_or_else(type_error)?;
            Ok((root_guard, call, entered, value))
        })();
        let delivery = DeliveryGuard {
            host: self.clone(),
            subscription,
        };
        match admitted {
            Ok((root, call, entered, value)) => {
                let host = self.clone();
                let receive = self.spawn_managed(call.id, async move {
                    let (_root, _delivery) = (root, delivery);
                    if let Err(error) = (record.handle)(entered, value).await {
                        host.0.errors.borrow_mut().push(error);
                    }
                    Ok(())
                });
                if let Err(error) = receive {
                    self.0.errors.borrow_mut().push(error);
                }
            }
            Err(error) => {
                self.0.errors.borrow_mut().push(error);
                drop(delivery);
            }
        }
        true
    }
}

struct DeliveryGuard {
    host: Host,
    subscription: core::Id,
}
impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        if let Err(error) = self.host.kernel().finish_delivery(self.subscription) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.flush();
    }
}
