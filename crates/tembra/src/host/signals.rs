//! Small adapter notifications consumed by the shared action driver.
use super::*;

/// Metadata notifications carry claim and root cleanup across an attached language adapter.
#[derive(Clone, Copy)]
pub enum ForeignNotification {
    RootEnded(core::Id),
    ReferenceReleased(core::Id),
}

impl Host {
    /// Deliver a kernel action owned by native callback storage.
    /// Returns true when the native adapter exclusively consumes the action.
    pub fn notify(&self, action: &core::Action) -> bool {
        match action {
            #[cfg(feature = "subscriptions")]
            core::Action::Deliver {
                subscription,
                operation,
                payload,
            } => return self.deliver(*subscription, *operation, *payload),
            #[cfg(feature = "subscriptions")]
            core::Action::DetachSubscription(subscription)
            | core::Action::SubscriptionFailed(subscription) => {
                return self.detach_listener(*subscription);
            }
            core::Action::StateChanged { object, .. } => self.publish_state(*object),
            core::Action::DisposeObject(object) => {
                self.0.observers.borrow_mut().remove(object);
            }
            core::Action::CancelRoot(root) => self.cancel_context(*root),
            core::Action::CancelTask(task) => self.cancel_task(*task),
            core::Action::ProgramComplete { program, error } => {
                if let Some(send) = self.0.programs.borrow_mut().remove(program) {
                    let _ = send.send(error.clone().map_or(Ok(()), Err));
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}

impl Host {
    pub(crate) fn observe(
        &self,
        object: core::Id,
        observer: std::rc::Weak<crate::readable::Observer>,
    ) {
        self.0
            .observers
            .borrow_mut()
            .entry(object)
            .or_default()
            .push(observer);
    }

    /// Publish a shared kernel state-change notification to native observers.
    pub fn publish_state(&self, object: core::Id) {
        let observers = {
            let mut indexed = self.0.observers.borrow_mut();
            let Some(observers) = indexed.get_mut(&object) else {
                return;
            };
            observers.retain(|observer| {
                observer
                    .upgrade()
                    .is_some_and(|observer| observer.active.get())
            });
            observers
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for observer in observers {
            if observer.active.get()
                && let Err(error) = (observer.invoke)()
            {
                self.0.errors.borrow_mut().push(error);
            }
        }
    }
}

impl Host {
    /// Attach the shared driver's wake callback. Native settlement requests another action drain.
    pub fn set_driver_wake(&self, wake: impl Fn() + 'static) {
        *self.0.driver_wake.borrow_mut() = Some(Rc::new(wake));
    }

    /// Attach metadata-only notifications for foreign root and reference cleanup.
    pub fn set_foreign_notifier(&self, notify: impl Fn(ForeignNotification) + 'static) {
        *self.0.foreign_notifier.borrow_mut() = Some(Rc::new(notify));
    }

    pub(super) fn notify_foreign(&self, notification: ForeignNotification) {
        let notify = self.0.foreign_notifier.borrow().clone();
        if let Some(notify) = notify {
            notify(notification);
        }
    }

    /// Attach the codec-bound service callback path provided by a foreign execution adapter.
    pub fn set_foreign_dispatcher(
        &self,
        dispatch: impl Fn(Value) -> LocalBoxFuture<'static, Result<Value>> + 'static,
    ) {
        *self.0.foreign_dispatcher.borrow_mut() = Some(Rc::new(dispatch));
    }

    pub(super) async fn foreign(&self, request: Value) -> Result<Value> {
        let dispatch = self
            .0
            .foreign_dispatcher
            .borrow()
            .clone()
            .ok_or_else(|| Error::invalid(Reason::Driver, "no foreign callback adapter"))?;
        dispatch(request).await
    }

    /// Wake an attached driver when native work settles without keeping idle infrastructure alive.
    pub fn poll_settled(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        self.0.settled_waker.register(cx.waker());
        if self.0.active.get() == 0 {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }

    /// Wait for native physical callbacks while the shared adapter continues driving kernel actions.
    pub async fn settled(&self) {
        while self.0.active.get() != 0 {
            let (send, receive) = oneshot::channel();
            self.0.settled.borrow_mut().push(send);
            let _ = receive.await;
        }
    }
}
