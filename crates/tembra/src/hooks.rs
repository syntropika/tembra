//! Typed hook contracts and sequential compositions. The kernel owns plans and invocations.
use crate::{Context, Error, Host, Result, core};
use futures::{FutureExt, future::LocalBoxFuture};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{any::TypeId, future::Future, rc::Rc};

pub(crate) type Compose<A, R> =
    Rc<dyn Fn(Host, Context, Rc<[core::Id]>, A) -> LocalBoxFuture<'static, Result<R>>>;
pub(crate) type Encode<R> = Rc<dyn Fn(R) -> Result<Option<Value>>>;
pub(crate) type Decode<R> = Rc<dyn Fn(Option<Value>) -> Result<R>>;
pub(crate) type LocalContribution = Rc<
    dyn Fn(
        Context,
        Box<dyn std::any::Any>,
    ) -> LocalBoxFuture<'static, Result<Box<dyn std::any::Any>>>,
>;
pub(crate) type PortableContribution =
    Rc<dyn Fn(Context, Value) -> LocalBoxFuture<'static, Result<Option<Value>>>>;

#[derive(Clone)]
pub(crate) struct Contribution {
    pub target: String,
    pub hook: String,
    pub signature: TypeId,
    pub priority: i32,
    pub local: LocalContribution,
    pub portable: PortableContribution,
}

/// A named, typed hook contract. Share this descriptor with contributing orbs.
/// Rust values remain local unless a participant belongs to another language.
///
/// ```
/// use tembra::{Hook, Orb};
/// let transform = Hook::<String, String>::waterfall("transform");
/// let provider = Orb::new("text", 1).hook(transform);
/// # let _ = provider;
/// ```
pub struct Hook<A, R> {
    pub(crate) name: String,
    pub(crate) compose: Compose<A, R>,
    pub(crate) encode: Encode<R>,
    pub(crate) decode: Decode<R>,
}

impl<A, R> Clone for Hook<A, R> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            compose: self.compose.clone(),
            encode: self.encode.clone(),
            decode: self.decode.clone(),
        }
    }
}

impl<A: Clone + Serialize + DeserializeOwned + 'static> Hook<A, ()> {
    /// Run participants in plan order with the same input, ignoring their results.
    pub fn series(name: impl Into<String>) -> Self {
        let decode: Decode<()> = Rc::new(|_| Ok(()));
        let participant = decode.clone();
        Self {
            name: name.into(),
            encode: Rc::new(|()| Ok(None)),
            decode,
            compose: Rc::new(move |host, ctx, plan, input: A| {
                let decode = participant.clone();
                async move {
                    for operation in plan.iter() {
                        host.hook_participant::<A, ()>(*operation, &ctx, input.clone(), &decode)
                            .await?;
                    }
                    Ok(())
                }
                .boxed_local()
            }),
        }
    }
}

impl<A: Serialize + DeserializeOwned + 'static> Hook<A, A> {
    /// Pass each result to the next participant; an empty plan returns the input.
    pub fn waterfall(name: impl Into<String>) -> Self {
        let decode: Decode<A> = Rc::new(|value| from_value(value.unwrap_or(Value::Null)));
        let participant = decode.clone();
        Self {
            name: name.into(),
            encode: Rc::new(|value| to_value(value).map(Some)),
            decode,
            compose: Rc::new(move |host, ctx, plan, mut input| {
                let decode = participant.clone();
                async move {
                    for operation in plan.iter() {
                        input = host
                            .hook_participant(*operation, &ctx, input, &decode)
                            .await?;
                    }
                    Ok(input)
                }
                .boxed_local()
            }),
        }
    }

    /// Pass each result to the next participant until `should_stop` accepts the accumulated value.
    ///
    /// # Guarantees
    ///
    /// Participants run sequentially in the versioned plan's priority and registration order.
    /// The predicate checks the initial input and each successful participant result. The first
    /// accepted value is returned unchanged, without invoking later participants. An empty plan
    /// returns the input. Native participants receive typed values without serialization;
    /// participants in another language use the same encoding as [`Self::waterfall`].
    ///
    /// Participant errors propagate without invoking later participants or checking the predicate
    /// again. Work already performed by earlier participants is not rolled back. Stopping is a
    /// normal successful result and does not cancel the context or change object ownership.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures::{FutureExt, executor::LocalPool};
    /// use tembra::{Hook, Host, Orb, Service, core::Requirement};
    ///
    /// let mut pool = LocalPool::new();
    /// let host = Host::new(pool.spawner());
    /// let hook = Hook::<i32, i32>::waterfall_until("limit", |value| *value >= 10);
    /// let declared = hook.clone();
    /// host.install(vec![
    ///     Orb::new("provider", 1).hook(hook.clone()).service(
    ///         Service::new("provider", move |self_| self_.hook(&declared))
    ///             .method("run", |hook, ctx, input: i32| async move {
    ///                 hook.run(&ctx, input).await
    ///             }),
    ///     ),
    ///     Orb::new("contributor", 1)
    ///         .requires(Requirement {
    ///             orb: "provider".into(), api_version: 1,
    ///             methods: Default::default(), hooks: ["limit".into()].into(),
    ///         })
    ///         .contribute("provider", &hook, 0, |_, value| async move { Ok(value + 10) })
    ///         .contribute("provider", &hook, 1, |_, _| async { panic!("already stopped") }),
    /// ]).unwrap();
    /// pool.run_until_stalled();
    /// let reference = host.get("provider", vec!["main".into()], "alice").unwrap();
    /// let result: i32 = pool.run_until(host.root("alice", move |ctx| {
    ///     async move { reference.call(&ctx, "run", 1).await }.boxed_local()
    /// })).unwrap();
    /// assert_eq!(result, 11);
    /// pool.run_until(host.close()).unwrap();
    /// ```
    pub fn waterfall_until(
        name: impl Into<String>,
        should_stop: impl Fn(&A) -> bool + 'static,
    ) -> Self {
        let decode: Decode<A> = Rc::new(|value| from_value(value.unwrap_or(Value::Null)));
        let participant = decode.clone();
        let should_stop = Rc::new(should_stop);
        Self {
            name: name.into(),
            encode: Rc::new(|value| to_value(value).map(Some)),
            decode,
            compose: Rc::new(move |host, ctx, plan, mut input| {
                let decode = participant.clone();
                let should_stop = should_stop.clone();
                async move {
                    if should_stop(&input) {
                        return Ok(input);
                    }
                    for operation in plan.iter() {
                        input = host
                            .hook_participant(*operation, &ctx, input, &decode)
                            .await?;
                        if should_stop(&input) {
                            break;
                        }
                    }
                    Ok(input)
                }
                .boxed_local()
            }),
        }
    }
}

impl<A, R> Hook<A, Option<R>>
where
    A: Clone + Serialize + DeserializeOwned + 'static,
    R: Serialize + DeserializeOwned + 'static,
{
    /// Return the first `Some` result. `None` corresponds to JavaScript `undefined`.
    /// A defined JavaScript `null` is a decision and is decoded as the chosen result type.
    pub fn bail(name: impl Into<String>) -> Self {
        let decode: Decode<Option<R>> = Rc::new(|value| value.map(from_value).transpose());
        let participant = decode.clone();
        Self {
            name: name.into(),
            encode: Rc::new(|value| value.map(to_value).transpose()),
            decode,
            compose: Rc::new(move |host, ctx, plan, input: A| {
                let decode = participant.clone();
                async move {
                    for operation in plan.iter() {
                        let result = host
                            .hook_participant(*operation, &ctx, input.clone(), &decode)
                            .await?;
                        if result.is_some() {
                            return Ok(result);
                        }
                    }
                    Ok(None)
                }
                .boxed_local()
            }),
        }
    }
}

impl<A: 'static, R: 'static> Hook<A, R> {
    /// Read the hook identifier within its declaring orb.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn signature(&self) -> TypeId {
        TypeId::of::<(A, R)>()
    }

    pub(crate) fn contribution<F, Fut>(
        &self,
        target: String,
        priority: i32,
        handle: F,
    ) -> Contribution
    where
        A: DeserializeOwned,
        F: Fn(Context, A) -> Fut + 'static,
        Fut: Future<Output = Result<R>> + 'static,
    {
        let handle = Rc::new(handle);
        let portable = handle.clone();
        let encode = self.encode.clone();
        Contribution {
            target,
            hook: self.name.clone(),
            signature: self.signature(),
            priority,
            local: Rc::new(move |ctx, input| {
                let input = input.downcast::<A>().map_err(|_| crate::orb::type_error());
                let handle = handle.clone();
                async move { Ok(Box::new(handle(ctx, *input?).await?) as Box<dyn std::any::Any>) }
                    .boxed_local()
            }),
            portable: Rc::new(move |ctx, input| {
                let input = serde_json::from_value(input)
                    .map_err(|error| Error::invalid(core::Reason::Arguments, error.to_string()));
                let (handle, encode) = (portable.clone(), encode.clone());
                async move { encode(handle(ctx, input?).await?) }.boxed_local()
            }),
        }
    }
}

/// A hook bound to its declaring service object. Keeping a handle does not retain the object.
pub struct HookHandle<A, R> {
    pub(crate) host: Host,
    pub(crate) orb: core::Id,
    pub(crate) target: String,
    pub(crate) object: Option<core::Id>,
    pub(crate) operation: core::Id,
    pub(crate) hook: Hook<A, R>,
}

impl<A, R> Clone for HookHandle<A, R> {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            orb: self.orb,
            target: self.target.clone(),
            object: self.object,
            operation: self.operation,
            hook: self.hook.clone(),
        }
    }
}

impl<A: 'static, R: 'static> HookHandle<A, R> {
    /// Execute the caller's versioned plan under a managed declaring-object invocation.
    /// Dropping this wait leaves physical execution owned by the host until it settles.
    pub async fn run(&self, context: &Context, input: A) -> Result<R> {
        if !self.host.same(&context.host) {
            return Err(Error::invalid(
                core::Reason::Context,
                "foreign host context",
            ));
        }
        context.check()?;
        self.host.ready_orb(self.orb).await?;
        let plan = self
            .host
            .kernel()
            .hook_plan(context.root, &self.target, &self.hook.name)?;
        let call = self.host.kernel().begin_operation_with_context(
            self.orb,
            context.root,
            self.operation,
            self.object,
            context.frame(),
        )?;
        let entered = self.host.context(call.root, Some(call.clone()))?;
        let (host, compose) = (self.host.clone(), self.hook.compose.clone());
        let receive = self.host.spawn_managed(call.id, async move {
            compose(host, entered, plan, input).await
        })?;
        receive
            .await
            .map_err(|_| Error::invalid(core::Reason::Executor, "hook executor stopped"))?
    }
}

pub(crate) fn to_value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value)
        .map_err(|error| Error::invalid(core::Reason::Result, error.to_string()))
}

pub(crate) fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|error| Error::invalid(core::Reason::Result, error.to_string()))
}
