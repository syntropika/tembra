use crate::{
    HookArgs, HookDescriptor, HookTerminal, HookType, RustHookDefinition, Strategy,
    descriptor::same,
    invalid,
    next::{Continuation, Frame},
};
use futures::{
    FutureExt,
    channel::oneshot,
    future::{LocalBoxFuture, poll_fn},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context as TaskContext, Poll},
};
use tembra_core::{
    CancellationToken, Context, Driver, FacetScope, RegisteredOperation, RegistrySnapshot, Result,
    RuntimeError,
};

/// Explicit bail control; false/null payloads are ordinary Stop values.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum BailDecision<R> {
    Continue,
    Stop(R),
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum BailOutcome<R> {
    Unresolved,
    Resolved(R),
}
/// Checked manager-local limits. Clones share the active physical-run budget.
#[derive(Clone, Debug)]
pub struct HookManagerOptions {
    pub max_contributions: usize,
    pub max_active_runs: usize,
}
impl Default for HookManagerOptions {
    fn default() -> Self {
        Self {
            max_contributions: 1024,
            max_active_runs: 128,
        }
    }
}
struct Budget {
    active: Cell<usize>,
    maximum: usize,
}
struct Permit(Rc<Budget>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.set(self.0.active.get() - 1);
    }
}
/// Composition over one Host's managed registries and existing local Driver.
///
/// Every strategy waits for each entered contribution/terminal's physical receipt as well
/// as its logical result. Sequential strategies do not start their next step before that
/// settlement. A handler's detached work must not depend on the enclosing pipeline result:
/// that creates an owned-work cycle. Cancellation signals never prove physical completion.
#[derive(Clone)]
pub struct HookManager {
    scope: FacetScope,
    driver: Rc<dyn Driver>,
    options: HookManagerOptions,
    budget: Rc<Budget>,
}
struct Plan {
    operations: Vec<RegisteredOperation>,
    _snapshot: RegistrySnapshot,
    driver: Weak<dyn Driver>,
    failure: Option<Rc<ParallelFailure>>,
}
#[derive(Default)]
struct ParallelFailure {
    error: RefCell<Option<RuntimeError>>,
    children: RefCell<Vec<CancellationToken>>,
}
impl ParallelFailure {
    fn record(&self, cause: RuntimeError) {
        if self.error.borrow().is_some() {
            return;
        }
        *self.error.borrow_mut() = Some(cause.clone());
        let children = self.children.borrow().clone();
        for token in children {
            token.cancel(cause.clone());
        }
    }
}
impl HookManager {
    /// Creates weak Host admission with bounded independently owned runs.
    /// # Errors
    /// Refuses limits outside `1..=4096` contributions or `1..=1024` active runs.
    pub fn new(
        scope: FacetScope,
        driver: Rc<dyn Driver>,
        options: HookManagerOptions,
    ) -> Result<Self> {
        if !(1..=4096).contains(&options.max_contributions)
            || !(1..=1024).contains(&options.max_active_runs)
        {
            return Err(invalid("hook_options", "hook limits leave checked range"));
        }
        Ok(Self {
            scope,
            driver,
            budget: Rc::new(Budget {
                active: Cell::new(0),
                maximum: options.max_active_runs,
            }),
            options,
        })
    }
    fn plan<A: 'static, H: 'static>(
        &self,
        definition: &HookDescriptor,
        strategy: Strategy,
        context: &Context,
    ) -> Result<Rc<Plan>> {
        if definition.strategy() != strategy {
            return Err(invalid("hook_strategy", "wrong strategy entry point"));
        }
        let snapshot = self
            .scope
            .registry(definition.registry())?
            .table(Some(context))?;
        if snapshot.len() > self.options.max_contributions {
            return Err(RuntimeError::unavailable(
                "hook_contributions",
                "too many hook contributions",
            ));
        }
        let expected = definition.contribution_member();
        let mut ordered = Vec::with_capacity(snapshot.len());
        for (ordinal, (key, entry)) in snapshot.entries().enumerate() {
            let data = entry
                .data()
                .as_object()
                .ok_or_else(|| invalid("hook_descriptor", "invalid contribution record"))?;
            if data.len() != 3
                || data.get("hook").and_then(serde_json::Value::as_str)
                    != Some(definition.fingerprint())
                || data.get("profile").and_then(serde_json::Value::as_u64) != Some(1)
            {
                return Err(RuntimeError::new(
                    tembra_core::ErrorCode::ContractMismatch,
                    "hook_descriptor",
                    "contribution descriptor differs",
                ));
            }
            let priority = data
                .get("priority")
                .and_then(serde_json::Value::as_i64)
                .filter(|v| v.unsigned_abs() <= 9_007_199_254_740_991)
                .ok_or_else(|| invalid("hook_priority", "invalid contribution priority"))?;
            let operation = entry.operation().ok_or_else(|| {
                invalid("hook_signature", "contribution has no managed operation")
            })?;
            let actual = operation
                .contract()
                .members("")
                .and_then(|members| members.get(operation.method()));
            if actual != Some(&expected) {
                return Err(invalid(
                    "hook_signature",
                    "contribution member profile differs",
                ));
            }
            operation.check_native::<HookArgs<A, H>, H>(context)?;
            ordered.push((priority, ordinal, key.to_owned(), operation.clone()));
        }
        ordered.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        Ok(Rc::new(Plan {
            operations: ordered
                .into_iter()
                .map(|(_, _, _, operation)| operation)
                .collect(),
            _snapshot: snapshot,
            driver: Rc::downgrade(&self.driver),
            failure: None,
        }))
    }
    fn terminal<A: 'static, R: 'static>(
        &self,
        definition: &HookDescriptor,
        terminal: Option<&HookTerminal<A, R>>,
        context: &Context,
    ) -> Result<()> {
        match (definition.terminal(), terminal) {
            (None, None) => Ok(()),
            (Some(expected), Some(terminal))
                if same(&expected.input, &terminal.profile().input)
                    && same(&expected.result, &terminal.profile().result) =>
            {
                terminal.operation.check_native::<A, R>(context)
            }
            _ => Err(invalid(
                "hook_signature",
                "terminal presence or profile differs",
            )),
        }
    }
    fn launch<T: 'static>(
        &self,
        context: &Context,
        build: impl FnOnce(&Self, &Context) -> Result<LocalBoxFuture<'static, Result<T>>> + 'static,
    ) -> HookRun<T> {
        let manager = self.clone();
        let context = context.clone();
        let cancellation = Rc::new(RefCell::new(None::<Rc<CancellationToken>>));
        let selected = cancellation.clone();
        HookRun {
            future: Some(
                async move {
                    context.check(manager.scope.host_id())?;
                    let (group, scope) = context.child_call(context.principal().clone())?;
                    let body = build(&manager, &group)?;
                    if manager.budget.active.get() >= manager.budget.maximum {
                        return Err(RuntimeError::unavailable(
                            "hook_runs",
                            "active hook run capacity exhausted",
                        ));
                    }
                    manager.budget.active.set(manager.budget.active.get() + 1);
                    let permit = Permit(manager.budget.clone());
                    let token = Rc::new(group.cancellation().clone());
                    let weak = Rc::downgrade(&token);
                    let settlement = context.subscribe_settle(move || {
                        if let Some(token) = weak.upgrade() {
                            token.cancel(RuntimeError::aborted(
                                "expired",
                                "hook caller root settled",
                            ));
                        }
                    })?;
                    *selected.borrow_mut() = Some(token.clone());
                    let (send, receive) = oneshot::channel();
                    manager.driver.spawn(
                        async move {
                            let result = body.await;
                            drop(settlement);
                            drop(scope);
                            drop(permit);
                            drop(token);
                            let _ = send.send(result);
                        }
                        .boxed_local(),
                    );
                    let result = receive.await.map_err(|_| {
                        RuntimeError::unavailable("hook_run", "hook coordinator ended")
                    })?;
                    let token = selected.borrow_mut().take();
                    drop(token);
                    result
                }
                .boxed_local(),
            ),
            cancellation,
        }
    }
    /// Runs ordered unit handlers without a terminal.
    /// # Errors
    /// Refuses full-plan admission before handler entry; later errors may follow effects.
    pub fn run_series<A: Clone + 'static>(
        &self,
        definition: &RustHookDefinition<A, ()>,
        input: A,
        context: &Context,
    ) -> HookRun<()> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal::<A, ()>(&definition.descriptor, None, context)?;
            definition.input.check(&input)?;
            let plan = manager.plan::<A, ()>(&definition.descriptor, Strategy::Series, context)?;
            let context = context.clone();
            Ok(async move {
                for operation in &plan.operations {
                    invoke(
                        operation,
                        &plan,
                        definition.input.clone(),
                        definition.result.clone(),
                        input.clone(),
                        &context,
                        None,
                    )
                    .await?;
                }
                Ok(())
            }
            .boxed_local())
        })
    }
    /// Runs ordered unit handlers and the exact declared managed terminal.
    /// # Errors
    /// Refuses terminal/profile/full-plan admission before any contribution effect.
    pub fn run_series_then<A: Clone + 'static, T: 'static>(
        &self,
        definition: &RustHookDefinition<A, ()>,
        input: A,
        context: &Context,
        terminal: HookTerminal<A, T>,
    ) -> HookRun<T> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal(&definition.descriptor, Some(&terminal), context)?;
            definition.input.check(&input)?;
            let plan = manager.plan::<A, ()>(&definition.descriptor, Strategy::Series, context)?;
            let context = context.clone();
            Ok(async move {
                for operation in &plan.operations {
                    invoke(
                        operation,
                        &plan,
                        definition.input.clone(),
                        definition.result.clone(),
                        input.clone(),
                        &context,
                        None,
                    )
                    .await?;
                }
                terminal.invoke(input, &context).await
            }
            .boxed_local())
        })
    }
    /// Returns the first explicit Stop, an exact terminal result, or Unresolved.
    /// # Errors
    /// Handler errors do not mean Continue; no later contribution starts after failure.
    pub fn run_bail<A: Clone + 'static, R: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        input: A,
        context: &Context,
        terminal: Option<HookTerminal<A, R>>,
    ) -> HookRun<BailOutcome<R>> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal(&definition.descriptor, terminal.as_ref(), context)?;
            definition.input.check(&input)?;
            let plan = manager.plan::<A, BailDecision<R>>(
                &definition.descriptor,
                Strategy::Bail,
                context,
            )?;
            let context = context.clone();
            let payload = definition.result.clone();
            let result = HookType::native(
                format!("tembra.hook.bail/{}@1", definition.descriptor.fingerprint()),
                move |decision: &BailDecision<R>| match decision {
                    BailDecision::Continue => Ok(()),
                    BailDecision::Stop(value) => payload.check(value),
                },
            )?;
            Ok(async move {
                for operation in &plan.operations {
                    match invoke(
                        operation,
                        &plan,
                        definition.input.clone(),
                        result.clone(),
                        input.clone(),
                        &context,
                        None,
                    )
                    .await?
                    {
                        BailDecision::Continue => {}
                        BailDecision::Stop(value) => return Ok(BailOutcome::Resolved(value)),
                    }
                }
                match terminal {
                    Some(terminal) => terminal
                        .invoke(input, &context)
                        .await
                        .map(BailOutcome::Resolved),
                    None => Ok(BailOutcome::Unresolved),
                }
            }
            .boxed_local())
        })
    }
    /// Moves each validated accumulator into the next handler.
    /// # Errors
    /// Invalid intermediate results stop before another handler starts.
    pub fn run_waterfall<A: 'static>(
        &self,
        definition: &RustHookDefinition<A, A>,
        input: A,
        context: &Context,
    ) -> HookRun<A> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal::<A, A>(&definition.descriptor, None, context)?;
            definition.input.check(&input)?;
            let plan =
                manager.plan::<A, A>(&definition.descriptor, Strategy::Waterfall, context)?;
            let context = context.clone();
            Ok(async move {
                let mut value = input;
                for operation in &plan.operations {
                    value = invoke(
                        operation,
                        &plan,
                        definition.input.clone(),
                        definition.result.clone(),
                        value,
                        &context,
                        None,
                    )
                    .await?;
                }
                Ok(value)
            }
            .boxed_local())
        })
    }
    /// Runs a waterfall followed by its declared ordinary terminal.
    /// # Errors
    /// Refuses plan/terminal mismatch or any invalid intermediate/output value.
    pub fn run_waterfall_then<A: 'static, T: 'static>(
        &self,
        definition: &RustHookDefinition<A, A>,
        input: A,
        context: &Context,
        terminal: HookTerminal<A, T>,
    ) -> HookRun<T> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal(&definition.descriptor, Some(&terminal), context)?;
            definition.input.check(&input)?;
            let plan =
                manager.plan::<A, A>(&definition.descriptor, Strategy::Waterfall, context)?;
            let context = context.clone();
            Ok(async move {
                let mut value = input;
                for operation in &plan.operations {
                    value = invoke(
                        operation,
                        &plan,
                        definition.input.clone(),
                        definition.result.clone(),
                        value,
                        &context,
                        None,
                    )
                    .await?;
                }
                terminal.invoke(value, &context).await
            }
            .boxed_local())
        })
    }
    /// Runs scoped middleware with an explicit managed terminal.
    /// # Errors
    /// Refuses admission, missing wrappers, expired/repeated Next and unacknowledged failure.
    /// Started downstream and physical work are joined before result completion.
    pub fn run_middleware<A: 'static, R: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        input: A,
        context: &Context,
        terminal: HookTerminal<A, R>,
    ) -> HookRun<R> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal(&definition.descriptor, Some(&terminal), context)?;
            definition.input.check(&input)?;
            let plan =
                manager.plan::<A, R>(&definition.descriptor, Strategy::Middleware, context)?;
            let context = context.clone();
            Ok(chain(plan, definition, terminal, 0, input, context))
        })
    }
    /// Starts independent handlers in order and returns registration-ordered results.
    /// # Errors
    /// First observed failure cancels only sibling child scopes and joins all started work.
    pub fn run_parallel<A: Clone + 'static, R: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        input: A,
        context: &Context,
    ) -> HookRun<Vec<R>> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal::<Vec<R>, Vec<R>>(&definition.descriptor, None, context)?;
            definition.input.check(&input)?;
            let plan = manager.plan::<A, R>(&definition.descriptor, Strategy::Parallel, context)?;
            let context = context.clone();
            Ok(parallel(plan, definition, input, context))
        })
    }
    /// Transforms an ordered successful parallel vector through its declared terminal.
    /// # Errors
    /// Refuses terminal/plan mismatch and skips terminal after any contribution failure.
    pub fn run_parallel_then<A: Clone + 'static, R: 'static, T: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        input: A,
        context: &Context,
        terminal: HookTerminal<Vec<R>, T>,
    ) -> HookRun<T> {
        let definition = definition.clone();
        self.launch(context, move |manager, context| {
            manager.terminal(&definition.descriptor, Some(&terminal), context)?;
            definition.input.check(&input)?;
            let plan = manager.plan::<A, R>(&definition.descriptor, Strategy::Parallel, context)?;
            let context = context.clone();
            Ok(async move {
                let values = parallel(plan, definition, input, context.clone()).await?;
                terminal.invoke(values, &context).await
            }
            .boxed_local())
        })
    }
}
async fn invoke<A: 'static, H: 'static>(
    operation: &RegisteredOperation,
    plan: &Plan,
    input: HookType<A>,
    result: HookType<H>,
    value: A,
    context: &Context,
    continuation: Option<Continuation<A, H>>,
) -> Result<H> {
    context.check(context.host_id())?;
    input.check(&value)?;
    let frame = Frame::new(input, result, continuation, plan.driver.clone());
    let call = operation.call_tracked::<HookArgs<A, H>, H>(context, HookArgs::new(value, &frame));
    let receipt = call.receipt();
    *frame.receipt.borrow_mut() = Some(receipt.clone());
    let outcome = std::panic::AssertUnwindSafe(call)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(invalid("hook_panic", "hook contribution panicked")));
    let outcome = outcome.and_then(|value| {
        if !frame.bound.get() {
            return Err(invalid(
                "hook_signature",
                "handler did not use checked hook wrapper",
            ));
        }
        // An outer Rust method can replace the wrapper's already checked value.
        // Validate before physical joining so invalid output promptly stops siblings.
        frame.result.check(&value)?;
        Ok(value)
    });
    frame.seal(outcome.as_ref().err());
    let joined = frame.join().await;
    // Signal sibling cancellation from the logical result, before physical leases
    // release the selected method's receipt. The whole run still joins that receipt.
    if let Some(failure) = &plan.failure
        && let Some(cause) = outcome.as_ref().err().or(joined.as_ref().err())
    {
        failure.record(cause.clone());
    }
    receipt.settled().await;
    let value = outcome?;
    joined?;
    Ok(value)
}
fn chain<A: 'static, R: 'static>(
    plan: Rc<Plan>,
    definition: RustHookDefinition<A, R>,
    terminal: HookTerminal<A, R>,
    index: usize,
    input: A,
    context: Context,
) -> LocalBoxFuture<'static, Result<R>> {
    async move {
        let Some(operation) = plan.operations.get(index) else {
            return terminal.invoke(input, &context).await;
        };
        let (next_plan, next_definition, next_terminal) =
            (plan.clone(), definition.clone(), terminal.clone());
        let next: Continuation<A, R> = Rc::new(move |input, context| {
            chain(
                next_plan.clone(),
                next_definition.clone(),
                next_terminal.clone(),
                index + 1,
                input,
                context,
            )
        });
        invoke(
            operation,
            &plan,
            definition.input.clone(),
            definition.result.clone(),
            input,
            &context,
            Some(next),
        )
        .await
    }
    .boxed_local()
}
fn parallel<A: Clone + 'static, R: 'static>(
    plan: Rc<Plan>,
    definition: RustHookDefinition<A, R>,
    input: A,
    context: Context,
) -> LocalBoxFuture<'static, Result<Vec<R>>> {
    async move {
        let failure = Rc::new(ParallelFailure::default());
        let plan = Rc::new(Plan {
            operations: plan.operations.clone(),
            _snapshot: plan._snapshot.clone(),
            driver: plan.driver.clone(),
            failure: Some(failure.clone()),
        });
        let mut futures = Vec::new();
        for operation in &plan.operations {
            let (child, guard) = context.child_call(context.principal().clone())?;
            failure
                .children
                .borrow_mut()
                .push(child.cancellation().clone());
            let (operation, plan, definition, input) = (
                operation.clone(),
                plan.clone(),
                definition.clone(),
                input.clone(),
            );
            futures.push(Some(
                async move {
                    let _guard = guard;
                    invoke(
                        &operation,
                        &plan,
                        definition.input,
                        definition.result,
                        input,
                        &child,
                        None,
                    )
                    .await
                }
                .boxed_local(),
            ));
        }
        let mut results: Vec<Option<R>> = (0..futures.len()).map(|_| None).collect();
        poll_fn(|cx| {
            for index in 0..futures.len() {
                let Some(future) = futures[index].as_mut() else {
                    continue;
                };
                if let Poll::Ready(result) = future.as_mut().poll(cx) {
                    drop(futures[index].take());
                    match result {
                        Ok(value) => results[index] = Some(value),
                        Err(cause) => {
                            failure.record(cause);
                        }
                    }
                }
            }
            if futures.iter().all(Option::is_none) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        let error = failure.error.borrow().clone();
        if let Some(error) = error {
            return Err(error);
        }
        Ok(results
            .into_iter()
            .map(|value| value.expect("successful parallel value"))
            .collect())
    }
    .boxed_local()
}
/// Caller result observer. Dropping it signals cancellation; the Driver still owns cleanup.
/// Every strategy joins actual physical entries, including independently retained leases.
///
/// # Panics
/// Polling again after `Ready` panics; it does not submit another pipeline.
#[must_use = "poll to admit a hook pipeline"]
pub struct HookRun<T> {
    future: Option<LocalBoxFuture<'static, Result<T>>>,
    cancellation: Rc<RefCell<Option<Rc<CancellationToken>>>>,
}
impl<T> Future for HookRun<T> {
    type Output = Result<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this
            .future
            .as_mut()
            .expect("hook result polled after completion")
            .as_mut()
            .poll(cx);
        if result.is_ready() {
            drop(this.future.take());
        }
        result
    }
}
impl<T> Drop for HookRun<T> {
    fn drop(&mut self) {
        drop(self.future.take());
        let token = self.cancellation.borrow_mut().take();
        if let Some(token) = token {
            token.cancel(RuntimeError::aborted(
                "hook_abandoned",
                "hook result observer dropped",
            ));
        }
    }
}
