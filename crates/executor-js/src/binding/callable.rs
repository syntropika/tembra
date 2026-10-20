use super::*;
use crate::engine::{CallableJob, Request};

pub(super) fn dispatch<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    cap: &Rc<Capability>,
    args: v8::Local<'s, v8::Array>,
) -> Result<v8::Local<'s, v8::Value>> {
    let resolver = v8::PromiseResolver::new(scope).ok_or_else(|| limit("promises"))?;
    let promise = resolver.get_promise(scope);
    let admission = (|| {
        let Kind::Callable(slot) = &cap.kind else {
            return Err(invalid("native_callable", "issued callable required"));
        };
        let module = cap.module.upgrade().ok_or_else(crate::slots::expired)?;
        let owner = cap.owner()?;
        slot.context.check(module.host)?;
        let lease = slot.invocation.retain(&slot.context)?;
        engine
            .admit_queue()
            .map_err(|error| error.with_details(json!({"callablePhase":"ingress"})))?;
        Ok((slot, module, owner, lease))
    })();
    let (slot, module, owner, lease) = match admission {
        Ok(admitted) => admitted,
        Err(error) => {
            super::projection::reject(engine, scope, resolver, &error)?;
            return Ok(promise.into());
        }
    };
    // Bounded ingress is reserved before arbitrary preparation. The raw view is
    // intentionally decoded later: intervening JS mutations remain observable.
    let mut reservation = QueueReservation {
        engine: engine.clone(),
        active: true,
    };
    let value = arg(scope, args, 0)?;
    let raw = JsValue {
        value: v8::Global::new(scope, value),
        module: module.clone(),
        native: None,
        setup: Some(owner.ancestry()),
    };
    let decode = Box::new(move || {
        let value =
            crate::binding::native_result(&raw.module.engine, &raw.module, raw.value.clone());
        drop(raw);
        value
    });
    let mut evaluator = |action: &mut dyn crate::scope::Action| action.run(scope);
    let (call, refusal) = crate::scope::within(engine.id, &mut evaluator, || {
        let mut call = slot.callback.invoke(decode);
        // No decoder or downstream future runs during scheduling acknowledgment.
        let refusal = call.admit().err();
        (call, refusal)
    });
    let job = CallableJob {
        resolver: v8::Global::new(scope, resolver),
        module,
        owner,
        lease,
        call,
        refusal,
    };
    reservation.active = false;
    if let Err(request) = engine.send_owned(Request::Callable(job)) {
        let engine = engine.clone();
        engine.driver.clone().spawn(
            async move {
                // Even an eager Driver may first poll us inside the op. Keep every
                // user capture until that runtime borrow has ended.
                futures::future::poll_fn(|cx| {
                    if engine.runtime.try_borrow_mut().is_ok() {
                        std::task::Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                })
                .await;
                engine.dequeued();
                if let Request::Callable(job) = *request {
                    crate::engine::pump::refuse_callable(&engine, job, crate::engine::closed());
                    // Resolving outside the pump schedules JS reactions but does not
                    // guarantee another reactor/channel wake after channel closure.
                    engine.pump_waker.wake();
                }
            }
            .boxed_local(),
        );
    }
    Ok(promise.into())
}

struct QueueReservation {
    engine: Rc<Engine>,
    active: bool,
}
impl Drop for QueueReservation {
    fn drop(&mut self) {
        if self.active {
            self.engine.dequeued();
        }
    }
}

#[cfg(test)]
mod tests;
