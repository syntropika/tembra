//! Portable language adapters over the existing typed hook manager and continuation protocol.
use crate::{
    callbacks::Callback,
    factory::{ContextLease, ObjectOwner, envelope, parse, payload},
    handles::Entry,
    session::Session,
};
use futures::FutureExt;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra_core::*;
use tembra_hooks::{portable::*, *};

pub(crate) trait Signature {
    fn install(
        &self,
        implementation: Implementation,
        name: String,
        callback: Rc<Callback>,
        session: &Rc<Session>,
        owner: Rc<ObjectOwner>,
    ) -> Implementation;
}
struct Typed<A, H> {
    types: std::marker::PhantomData<fn(A) -> H>,
    unit_bail: bool,
}
impl<A: PortableValue, H: PortableValue> Signature for Typed<A, H> {
    fn install(
        &self,
        implementation: Implementation,
        name: String,
        callback: Rc<Callback>,
        session: &Rc<Session>,
        owner: Rc<ObjectOwner>,
    ) -> Implementation {
        let unit_bail = self.unit_bail;
        let weak = Rc::downgrade(session);
        let handler: MethodHandler<HookArgs<A, H>, H> = Rc::new(move |args, context| {
            let weak = weak.clone();
            let callback = callback.clone();
            let invocation = owner.invocation.clone();
            async move {
                let session = weak
                    .upgrade()
                    .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
                let physical = invocation.retain(&context)?;
                let lease = ContextLease::issue(&session, context, Some(invocation))?;
                let (input, next) = args.into_parts();
                let mut raw = json!({"input":wire(input.encode()?)});
                if let Some(next) = next {
                    let continuation = Rc::new(ContinuationImpl { next });
                    raw["next"] =
                        json!({"$next":session.issue(Entry::Continuation(continuation))?});
                }
                let (send, receive) = futures::channel::oneshot::channel();
                session.scheduler.driver.clone().spawn(
                    async move {
                        let _physical = physical;
                        let result = async {
                            let value = session
                                .invoke(
                                    callback,
                                    vec![lease.wire.clone(), raw],
                                    false,
                                    false,
                                    if unit_bail { 2 } else { 1 },
                                )
                                .await?;
                            H::decode(payload(parse(&value)?)?)
                        }
                        .await;
                        let _ = send.send(result);
                    }
                    .boxed_local(),
                );
                receive
                    .await
                    .map_err(|_| RuntimeError::unavailable("closed", "hook callback ended"))?
            }
            .boxed_local()
        });
        let wrapper = hook_wrapper();
        implementation.method(name, move |args, context| {
            wrapper(args, context, handler.clone())
        })
    }
}
pub(crate) trait Continuation {
    fn prepare(&self, input: Rc<Callback>, recover: bool) -> ProjectedCall<Value>;
}
struct ContinuationImpl<A, H> {
    next: Next<A, H>,
}
impl<A: PortableValue, H: PortableValue> Continuation for ContinuationImpl<A, H> {
    fn prepare(&self, input: Rc<Callback>, recover: bool) -> ProjectedCall<Value> {
        let decode = move || payload(parse(&input.sync(vec![])?)?);
        let encode = |value: H| Ok(wire(value.encode()?));
        if recover {
            self.next.recover_with(move || H::decode(decode()?), encode)
        } else {
            self.next.call_with(move || A::decode(decode()?), encode)
        }
    }
}
pub(crate) fn wire(value: Option<Value>) -> Value {
    match value {
        Some(v) => json!({"$portable":v.to_string()}),
        None => json!({"$undefined":true}),
    }
}
pub(crate) struct Adapter<'a> {
    pub session: &'a Session,
}
impl PortableHookAdapter for Adapter<'_> {
    fn register<A: PortableValue, H: PortableValue>(&self, d: &HookDescriptor) -> Result<()> {
        self.session.hook_signatures.borrow_mut().insert(
            d.fingerprint().to_owned(),
            Rc::new(Typed::<A, H> {
                types: std::marker::PhantomData,
                unit_bail: d.strategy() == Strategy::Bail && d.result().is_none(),
            }),
        );
        Ok(())
    }
}
pub(crate) fn signature(
    session: &Session,
    member: &tembra_contracts::Member,
) -> Result<Rc<dyn Signature>> {
    let tembra_contracts::Member::Method {
        args: Some(tembra_contracts::ValueSchema::Native(id)),
        ..
    } = member
    else {
        return Err(RuntimeError::invalid(
            "native_signature",
            "unsupported native signature",
        ));
    };
    let fingerprint = id
        .strip_prefix("tembra.hook.args/")
        .and_then(|s| s.strip_suffix("@1"))
        .ok_or_else(|| {
            RuntimeError::invalid("native_signature", "native types require a Rust adapter")
        })?;
    session
        .hook_signatures
        .borrow()
        .get(fingerprint)
        .cloned()
        .ok_or_else(|| {
            RuntimeError::invalid(
                "native_signature",
                "hook must be registered before installing its facets",
            )
        })
}
pub(crate) fn register(session: &Session, value: &Value) -> Result<()> {
    let descriptor = HookDescriptor::from_json(crate::factory::text(value)?)?;
    let key = descriptor.fingerprint().to_owned();
    if session.hooks.borrow().contains_key(&key) {
        return Ok(());
    }
    if session.hooks.borrow().len() >= 1024 {
        return Err(RuntimeError::unavailable(
            "hook_limit",
            "hook registration capacity exhausted",
        ));
    }
    let hook = PortableHook::register(descriptor.clone(), &Adapter { session })?;
    session.hooks.borrow_mut().insert(key, hook);
    Ok(())
}
pub(crate) fn run(
    session: &Rc<Session>,
    scope: FacetScope,
    id: &str,
    descriptor: &Value,
    token: &Value,
    input: &Value,
    terminal: &Value,
) -> Result<Value> {
    let descriptor = HookDescriptor::from_json(crate::factory::text(descriptor)?)?;
    let hook = session
        .hooks
        .borrow()
        .get(descriptor.fingerprint())
        .cloned()
        .ok_or_else(|| RuntimeError::invalid("hook_signature", "hook is not registered"))?;
    let context = crate::operations::context(session, token)?;
    let physical = context
        .invocation
        .as_ref()
        .map(|owner| owner.retain(&context.context))
        .transpose()?;
    let operation = if crate::factory::undefined(terminal) {
        None
    } else {
        match session.entry(crate::factory::handle(terminal)?)? {
            Entry::Operation(op) => Some(op),
            _ => {
                return Err(RuntimeError::invalid(
                    "hook_signature",
                    "terminal must be an issued managed operation",
                ));
            }
        }
    };
    // Each issued facet scope shares its manager clones and active physical-run budget.
    let manager = if let Some(manager) = session.hook_managers.borrow().get(id) {
        manager.clone()
    } else {
        HookManager::new(
            scope,
            session.scheduler.driver.clone(),
            HookManagerOptions::default(),
        )?
    };
    session
        .hook_managers
        .borrow_mut()
        .insert(id.to_owned(), manager.clone());
    let input = payload(parse(input)?)?;
    session.start(
        async move {
            // An abandoned JavaScript promise must not release the originating receiver while
            // hook callbacks still run. Keep its lease through portable result extraction.
            let _physical = physical;
            Ok(json!(
                envelope(hook.run(manager, input, context.context, operation).await?).to_string()
            ))
        }
        .boxed_local(),
    )
}

pub(crate) fn register_opaque(session: &Session, id: &str) -> Result<()> {
    session.opaque.borrow_mut().insert(id.to_owned());
    Ok(())
}
