use crate::{HookType, invalid};
use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};
use tembra_core::{
    CancellationToken, Context, Driver, InvocationReceipt, Result, RuntimeError, ScopeGuard,
};

pub(crate) type Continuation<A, R> = Rc<dyn Fn(A, Context) -> LocalBoxFuture<'static, Result<R>>>;
enum Attempt {
    Unused,
    Reserved,
    Started,
    Done(Result<()>),
}
struct Output {
    started: Cell<bool>,
    delegated: Cell<bool>,
    done: Cell<bool>,
    error: RefCell<Option<RuntimeError>>,
}
pub(crate) struct Frame<A, R> {
    pub input: HookType<A>,
    pub result: HookType<R>,
    pub receipt: RefCell<Option<InvocationReceipt>>,
    context: RefCell<Option<Context>>,
    next_context: RefCell<Option<Context>>,
    next_scope: RefCell<Option<ScopeGuard>>,
    continuation: Option<Continuation<A, R>>,
    driver: Weak<dyn Driver>,
    pub bound: Cell<bool>,
    sealed: Cell<bool>,
    attempt: RefCell<Attempt>,
    recovered: Cell<bool>,
    preparing: Cell<bool>,
    outputs: RefCell<Vec<Rc<Output>>>,
    changed: RefCell<CancellationToken>,
}
impl<A: 'static, R: 'static> Frame<A, R> {
    pub(crate) fn new(
        input: HookType<A>,
        result: HookType<R>,
        continuation: Option<Continuation<A, R>>,
        driver: Weak<dyn Driver>,
    ) -> Rc<Self> {
        Rc::new(Self {
            input,
            result,
            receipt: RefCell::default(),
            context: RefCell::default(),
            next_context: RefCell::default(),
            next_scope: RefCell::default(),
            continuation,
            driver,
            bound: Cell::new(false),
            sealed: Cell::new(false),
            attempt: RefCell::new(Attempt::Unused),
            recovered: Cell::new(false),
            preparing: Cell::new(false),
            outputs: RefCell::default(),
            changed: RefCell::new(CancellationToken::new()),
        })
    }
    fn notify(&self) {
        let previous = self.changed.replace(CancellationToken::new());
        previous.cancel("hook transition");
    }
    pub(crate) fn bind(&self, context: &Context) -> Result<()> {
        if self.bound.get()
            || self.sealed.get()
            || !self
                .receipt
                .borrow()
                .as_ref()
                .is_some_and(|receipt| receipt.matches_context(context))
        {
            return Err(invalid(
                "hook_next_scope",
                "handler requires its exact tracked receiver",
            ));
        }
        context.check(context.host_id())?;
        let (next, guard) = context.child_call(context.principal().clone())?;
        *self.context.borrow_mut() = Some(context.clone());
        *self.next_context.borrow_mut() = Some(next);
        *self.next_scope.borrow_mut() = Some(guard);
        self.bound.set(true);
        Ok(())
    }
    fn active(&self) -> Result<Context> {
        if self.sealed.get() {
            return Err(expired());
        }
        let context = self.context.borrow().clone().ok_or_else(|| {
            invalid(
                "hook_next_scope",
                "Next is not bound by its handler wrapper",
            )
        })?;
        context.check(context.host_id())?;
        if self.preparing.get() {
            return Err(invalid(
                "hook_reentrancy",
                "hook validation cannot reenter its frame",
            ));
        }
        Ok(context)
    }
    pub(crate) fn next(self: &Rc<Self>) -> Option<Next<A, R>> {
        self.continuation.as_ref().map(|_| Next {
            frame: Rc::downgrade(self),
        })
    }
    pub(crate) fn seal(&self, error: Option<&RuntimeError>) {
        self.sealed.set(true);
        if matches!(*self.attempt.borrow(), Attempt::Reserved) {
            *self.attempt.borrow_mut() = Attempt::Done(Err(abandoned()));
        }
        for output in self.outputs.borrow().iter() {
            if !output.started.get() {
                output.done.set(true);
            }
        }
        let context = self.next_context.borrow().clone();
        if let Some(error) = error
            && let Some(context) = context
        {
            context.cancel(error.clone());
        }
        self.notify();
    }
    pub(crate) async fn join(&self) -> Result<()> {
        loop {
            let wait = self.changed.borrow().clone();
            let pending = matches!(*self.attempt.borrow(), Attempt::Started | Attempt::Reserved)
                || self
                    .outputs
                    .borrow()
                    .iter()
                    .any(|output| !output.done.get());
            if !pending {
                if let Some(error) = self
                    .outputs
                    .borrow()
                    .iter()
                    .find_map(|output| output.error.borrow().clone())
                {
                    return Err(error);
                }
                if !self.recovered.get()
                    && let Attempt::Done(Err(error)) = &*self.attempt.borrow()
                {
                    return Err(error.clone());
                }
                return Ok(());
            }
            wait.cancelled().await;
        }
    }
    fn abandon(&self) {
        if matches!(*self.attempt.borrow(), Attempt::Reserved | Attempt::Started) {
            *self.attempt.borrow_mut() = Attempt::Done(Err(abandoned()));
            self.notify();
        }
    }
    fn output(self: &Rc<Self>, project: bool) -> OutputCompletion {
        if !project {
            return OutputCompletion {
                finish: None,
                output: None,
                admit: None,
                refuse: None,
            };
        }
        let output = Rc::new(Output {
            started: Cell::new(false),
            delegated: Cell::new(false),
            done: Cell::new(false),
            error: RefCell::default(),
        });
        self.outputs.borrow_mut().push(output.clone());
        let weak = Rc::downgrade(self);
        let selected = output.clone();
        OutputCompletion {
            output: Some(output.clone()),
            admit: None,
            refuse: None,
            finish: Some(Box::new(move |result| {
                if !selected.done.replace(true) {
                    *selected.error.borrow_mut() = result.err();
                    if let Some(frame) = weak.upgrade() {
                        frame.notify();
                    }
                }
            })),
        }
    }
    fn recovery<T>(
        &self,
        decode: impl FnOnce() -> Result<R>,
        encode: impl FnOnce(R) -> Result<T>,
        admitted: bool,
    ) -> Result<T> {
        if admitted {
            let context = self.context.borrow().clone().ok_or_else(expired)?;
            context.check(context.host_id())?;
            if self.preparing.get() {
                return Err(invalid(
                    "hook_reentrancy",
                    "hook validation cannot reenter its frame",
                ));
            }
        } else {
            self.active()?;
        }
        if self.recovered.get() || !matches!(*self.attempt.borrow(), Attempt::Done(Err(_))) {
            return Err(invalid(
                "hook_recovery",
                "recovery requires one settled failed Next",
            ));
        }
        self.preparing.set(true);
        let guard = Preparing(&self.preparing);
        let fallback = decode()?;
        self.result.check(&fallback)?;
        let output = encode(fallback)?;
        let context = self.context.borrow().clone().ok_or_else(expired)?;
        if self.sealed.get() && !admitted {
            return Err(expired());
        }
        context.check(context.host_id())?;
        self.recovered.set(true);
        drop(guard);
        self.notify();
        Ok(output)
    }
}
struct Preparing<'a>(&'a Cell<bool>);
impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
fn expired() -> RuntimeError {
    RuntimeError::aborted("expired", "hook handler scope ended")
}
fn abandoned() -> RuntimeError {
    RuntimeError::aborted("hook_next_abandoned", "reserved Next was never started")
}

/// One shared, scope-bound continuation attempt. Clones do not grant another use.
pub struct Next<A, R> {
    frame: Weak<Frame<A, R>>,
}
impl<A, R> Clone for Next<A, R> {
    fn clone(&self) -> Self {
        Self {
            frame: self.frame.clone(),
        }
    }
}
impl<A: 'static, R: 'static> Next<A, R> {
    /// Reserves one attempt immediately; polling starts owned downstream work.
    /// # Errors
    /// Refuses expired/unbound/cancelled frames and repeated use.
    /// Started work survives observer Drop. Dropping or retaining an unpolled reservation
    /// fails the primary attempt with `hook_next_abandoned`; sealing cannot wait forever
    /// for a future that never started. It can be recovered only while the handler is active.
    pub fn call(&self, input: A) -> LocalBoxFuture<'static, Result<R>> {
        self.attempt(move || Ok(input), Ok, false).future
    }
    /// Explicitly acknowledges a failed Next with a validated moved fallback.
    /// # Errors
    /// Refuses inactive, reentrant, successful/in-flight Next and duplicate recovery.
    /// Invalid fallback leaves acknowledgment available; external validator effects remain.
    pub fn recover(&self, fallback: R) -> Result<R> {
        self.frame
            .upgrade()
            .ok_or_else(expired)?
            .recovery(|| Ok(fallback), Ok, false)
    }
    /// Reserves before deferred conversion and tracks final foreign projection separately.
    /// # Errors
    /// Returned future reports attempt/codec errors. Dropping its completion without
    /// preparing the actual outcome records a sticky projection failure. A trusted backend
    /// calls [`OutputCompletion::admit`] after scheduling admission, before handler seal.
    /// Ordinary Rust unpolled reservations retain [`Self::call`]'s abandonment behavior.
    pub fn call_with<T: 'static>(
        &self,
        decode: impl FnOnce() -> Result<A> + 'static,
        encode: impl FnOnce(R) -> Result<T> + 'static,
    ) -> ProjectedCall<T> {
        self.attempt(decode, encode, true)
    }
    /// Prepares and validates a fallback before committing recovery acknowledgment.
    /// # Errors
    /// Failed preparation leaves acknowledgment unused; later projection cannot roll it back.
    /// Dropping the projection completion after admission is a separate sticky failure.
    pub fn recover_with<T: 'static>(
        &self,
        decode: impl FnOnce() -> Result<R> + 'static,
        encode: impl FnOnce(R) -> Result<T> + 'static,
    ) -> ProjectedCall<T> {
        let Some(frame) = self.frame.upgrade() else {
            return ProjectedCall::error(expired());
        };
        let mut completion = frame.output(true);
        let admitted = Rc::new(Cell::new(false));
        let admission = admitted.clone();
        let weak = Rc::downgrade(&frame);
        completion.admit = Some(Box::new(move || {
            weak.upgrade().ok_or_else(expired)?.active()?;
            admission.set(true);
            Ok(())
        }));
        let output = completion.output.clone();
        let frame = Rc::downgrade(&frame);
        ProjectedCall {
            future: async move {
                let frame = frame.upgrade().ok_or_else(expired)?;
                if let Some(output) = output {
                    output.started.set(true);
                }
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    frame.recovery(decode, encode, admitted.get())
                }))
                .unwrap_or_else(|_| {
                    Err(invalid("hook_panic", "hook recovery preparation panicked"))
                })
            }
            .boxed_local(),
            completion,
        }
    }
    fn attempt<T: 'static>(
        &self,
        decode: impl FnOnce() -> Result<A> + 'static,
        encode: impl FnOnce(R) -> Result<T> + 'static,
        project: bool,
    ) -> ProjectedCall<T> {
        let Some(frame) = self.frame.upgrade() else {
            return ProjectedCall::error(expired());
        };
        if let Err(error) = frame.active() {
            return ProjectedCall::error(error);
        }
        if !matches!(*frame.attempt.borrow(), Attempt::Unused) {
            return ProjectedCall::refusal(
                invalid("hook_next_used", "Next was already consumed"),
                frame.output(project),
            );
        }
        *frame.attempt.borrow_mut() = Attempt::Reserved;
        let mut completion = frame.output(project);
        let admitted = Rc::new(Cell::new(false));
        let admission = admitted.clone();
        let weak_admission = Rc::downgrade(&frame);
        completion.admit = Some(Box::new(move || {
            let frame = weak_admission.upgrade().ok_or_else(expired)?;
            frame.active()?;
            if !matches!(*frame.attempt.borrow(), Attempt::Reserved) {
                return Err(expired());
            }
            *frame.attempt.borrow_mut() = Attempt::Started;
            admission.set(true);
            Ok(())
        }));
        let weak_refusal = Rc::downgrade(&frame);
        let refusal_projection = completion.output.clone();
        completion.refuse = Some(Box::new(move |cause| {
            if !refusal_projection
                .as_ref()
                .is_some_and(|output| output.delegated.get())
                && let Some(frame) = weak_refusal.upgrade()
                && matches!(
                    *frame.attempt.borrow(),
                    Attempt::Reserved | Attempt::Started
                )
            {
                *frame.attempt.borrow_mut() = Attempt::Done(Err(cause));
                frame.notify();
            }
        }));
        let output = completion.output.clone();
        let reservation = Reservation {
            frame: Rc::downgrade(&frame),
            started: false,
        };
        let weak = Rc::downgrade(&frame);
        ProjectedCall {
            completion,
            future: async move {
                let mut reservation = reservation;
                let frame = weak.upgrade().ok_or_else(expired)?;
                let preflight = (|| {
                    if admitted.get() {
                        let context = frame.next_context.borrow().clone().ok_or_else(expired)?;
                        context.check(context.host_id())?;
                        if !matches!(*frame.attempt.borrow(), Attempt::Started) {
                            return Err(expired());
                        }
                    } else {
                        frame.active()?;
                        if !matches!(*frame.attempt.borrow(), Attempt::Reserved) {
                            return Err(expired());
                        }
                    }
                    let driver = frame.driver.upgrade().ok_or_else(|| {
                        tembra_core::RuntimeError::unavailable("closed", "hook Driver ended")
                    })?;
                    let context = frame.next_context.borrow().clone().ok_or_else(expired)?;
                    Ok((driver, context))
                })();
                let (driver, context) = match preflight {
                    Ok(admission) => admission,
                    Err(cause) => {
                        reservation.fail(cause.clone());
                        return Err(cause);
                    }
                };
                *frame.attempt.borrow_mut() = Attempt::Started;
                reservation.started = true;
                if let Some(output) = output {
                    output.started.set(true);
                    output.delegated.set(true);
                }
                let continuation = frame.continuation.clone().expect("Next has a continuation");
                let (send, receive) = oneshot::channel();
                let mut settlement = StartedSettlement {
                    frame: frame.clone(),
                    finished: false,
                };
                driver.spawn(
                    async move {
                        let result = std::panic::AssertUnwindSafe(async {
                            let input = decode()?;
                            frame.input.check(&input)?;
                            context.check(context.host_id())?;
                            let value = continuation(input, context).await?;
                            frame.result.check(&value)?;
                            encode(value)
                        })
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            Err(invalid(
                                "hook_panic",
                                "hook continuation preparation or callback panicked",
                            ))
                        });
                        settlement.finish(result.as_ref().map(|_| ()).map_err(Clone::clone));
                        let _ = send.send(result);
                    }
                    .boxed_local(),
                );
                receive.await.map_err(|_| abandoned())?
            }
            .boxed_local(),
        }
    }
}
struct StartedSettlement<A: 'static, R: 'static> {
    frame: Rc<Frame<A, R>>,
    finished: bool,
}
impl<A: 'static, R: 'static> StartedSettlement<A, R> {
    fn finish(&mut self, result: Result<()>) {
        self.finished = true;
        if matches!(*self.frame.attempt.borrow(), Attempt::Started) {
            *self.frame.attempt.borrow_mut() = Attempt::Done(result);
        }
        self.frame.notify();
    }
}
impl<A: 'static, R: 'static> Drop for StartedSettlement<A, R> {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(Err(abandoned()));
        }
    }
}

struct Reservation<A: 'static, R: 'static> {
    frame: Weak<Frame<A, R>>,
    started: bool,
}
impl<A: 'static, R: 'static> Reservation<A, R> {
    fn fail(&mut self, cause: RuntimeError) {
        self.started = true;
        if let Some(frame) = self.frame.upgrade() {
            if matches!(
                *frame.attempt.borrow(),
                Attempt::Reserved | Attempt::Started
            ) {
                *frame.attempt.borrow_mut() = Attempt::Done(Err(cause));
            }
            frame.notify();
        }
    }
}
impl<A: 'static, R: 'static> Drop for Reservation<A, R> {
    fn drop(&mut self) {
        if !self.started
            && let Some(frame) = self.frame.upgrade()
        {
            frame.abandon();
        }
    }
}

/// Future data plus a one-shot backend projection observation. Neither is a new invocation.
pub struct ProjectedCall<T> {
    future: LocalBoxFuture<'static, Result<T>>,
    completion: OutputCompletion,
}
impl<T: 'static> ProjectedCall<T> {
    fn error(error: RuntimeError) -> Self {
        Self::refusal(
            error,
            OutputCompletion {
                finish: None,
                output: None,
                admit: None,
                refuse: None,
            },
        )
    }
    fn refusal(error: RuntimeError, completion: OutputCompletion) -> Self {
        let output = completion.output.clone();
        Self {
            future: async move {
                if let Some(output) = output {
                    output.started.set(true);
                }
                Err(error)
            }
            .boxed_local(),
            completion,
        }
    }
    pub fn into_parts(self) -> (LocalBoxFuture<'static, Result<T>>, OutputCompletion) {
        (self.future, self.completion)
    }
}
/// Trusted backend acknowledgment of final checked projection, not the Next outcome.
#[must_use = "complete after projection, including successful preparation of a recorded rejection"]
pub struct OutputCompletion {
    output: Option<Rc<Output>>,
    admit: Option<Box<dyn FnOnce() -> Result<()>>>,
    refuse: Option<Box<dyn FnOnce(RuntimeError)>>,
    finish: Option<Box<dyn FnOnce(Result<()>)>>,
}
impl OutputCompletion {
    /// Marks this callable accepted by a trusted backend queue, without decoding or polling.
    /// A queued primary attempt is joined even if the handler returns before its first poll.
    /// After successful admission the backend must poll or drop the associated future.
    /// Retaining it unpolled retains the started attempt, receiver, run permit and physical
    /// join; the wrapper and Host closure can remain pending indefinitely.
    ///
    /// # Errors
    /// Refuses ended/cancelled primary or recovery admission before scheduling work.
    pub fn admit(&mut self) -> Result<()> {
        if let Some(admit) = self.admit.take() {
            admit()?;
        }
        if let Some(output) = &self.output {
            output.started.set(true);
        }
        Ok(())
    }
    /// Records a pre-execution backend refusal as the primary attempt's recoverable error.
    /// Secondary misuse and recovery refusals have no authority to alter that primary.
    /// The backend still completes successful typed rejection projection with `Ok(())`.
    pub fn refuse(&mut self, cause: RuntimeError) {
        if let Some(refuse) = self.refuse.take() {
            refuse(cause);
        }
    }
    pub fn complete(mut self, projection: Result<()>) {
        if let Some(finish) = self.finish.take() {
            finish(projection);
        }
    }
}
impl Drop for OutputCompletion {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish(Err(invalid(
                "callable_projection",
                "admitted output projection was abandoned",
            )));
        }
    }
}

/// Actual method argument carrier; retained copies cannot extend its frame authority.
pub struct HookArgs<A, H> {
    input: A,
    next: Option<Next<A, H>>,
    pub(crate) frame: Weak<Frame<A, H>>,
}
impl<A, H> HookArgs<A, H> {
    pub fn input(&self) -> &A {
        &self.input
    }
    pub fn next(&self) -> Option<&Next<A, H>> {
        self.next.as_ref()
    }
    pub fn into_parts(self) -> (A, Option<Next<A, H>>) {
        (self.input, self.next)
    }
    pub(crate) fn new(input: A, frame: &Rc<Frame<A, H>>) -> Self
    where
        A: 'static,
        H: 'static,
    {
        Self {
            input,
            next: frame.next(),
            frame: Rc::downgrade(frame),
        }
    }
}
