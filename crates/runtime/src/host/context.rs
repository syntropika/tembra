//! A logical root stays pinned until physical work settles.

use super::*;

impl Host {
    /// Issue a context for an active root and matching invocation.
    pub fn context(&self, root: core::Id, invocation: Option<core::Invocation>) -> Result<Context> {
        self.kernel().check_root(root)?;
        if invocation.as_ref().is_some_and(|call| call.root != root) {
            return Err(Error::invalid(
                Reason::Context,
                "invocation belongs to another root",
            ));
        }
        Ok(Context {
            host: self.clone(),
            root,
            frame: invocation.as_ref().map_or(0, |call| call.context),
            call: invocation,
        })
    }

    /// Run an operation while a guard retains its root lifetime.
    pub async fn root<T>(
        &self,
        principal: &str,
        operation: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>>,
    ) -> Result<T> {
        let root = self.kernel().begin_root(principal)?;
        let context = self.context(root, None)?;
        let _guard = RootGuard {
            host: self.clone(),
            root,
        };
        operation(context).await
    }
}

impl Host {
    /// Run a root operation as a task of this host instead of the caller's future.
    ///
    /// The root lives until the operation itself completes, whoever (if anyone) awaits the
    /// returned receiver: dropping it neither ends the root nor stops the operation. The task is
    /// tracked like every callback, so [`Host::close`] waits for it. Use it when work admitted
    /// under the root must outlive the caller's own wait, for example a response that keeps
    /// streaming after its headers were handed to the application.
    pub fn spawn_root<T: 'static>(
        &self,
        principal: &str,
        operation: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>> + 'static,
    ) -> Result<futures::channel::oneshot::Receiver<Result<T>>> {
        let host = self.clone();
        let principal = principal.to_owned();
        self.spawn(async move { host.root(&principal, operation).await })
    }
}

pub(super) struct RootGuard {
    pub(super) host: Host,
    pub(super) root: core::Id,
}

impl Drop for RootGuard {
    /// End the root's logical lifetime and dispatch pending cleanup.
    fn drop(&mut self) {
        if let Err(error) = self.host.kernel().end_root(self.root) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.end_context(self.root);
        self.host
            .notify_foreign(super::signals::ForeignNotification::RootEnded(self.root));
        self.host.flush();
    }
}

/// Contexts are issued by a host, and become unusable when their root or invocation ends.
#[derive(Clone)]
pub struct Context {
    frame: core::Id,
    pub(crate) host: Host,
    pub root: core::Id,
    call: Option<core::Invocation>,
}

impl Context {
    /// Read the frame identity for local adapter calls.
    pub fn frame(&self) -> core::Id {
        self.frame
    }

    /// Derive values without extending identity or invocation lifetime.
    pub fn with<T: core::ContextDatum>(&self, key: &core::ContextKey<T>, value: T) -> Result<Self> {
        self.check()?;
        let frame = self.host.kernel().derive_context(
            self.root,
            self.frame,
            self.call.as_ref().map(|call| call.id),
            key.contract(),
            value.into_value(),
        )?;
        Ok(Self {
            host: self.host.clone(),
            root: self.root,
            call: self.call.clone(),
            frame,
        })
    }

    /// Retain the admitted isolated revision in this invocation's private execution ancestry.
    pub fn enter_execution(&self) -> Result<Self> {
        let call = self.invocation()?;
        let frame = self.host.kernel().enter_execution(call.id, self.frame)?;
        Ok(Self {
            host: self.host.clone(),
            root: self.root,
            call: self.call.clone(),
            frame,
        })
    }

    /// Read typed metadata while this invocation remains valid.
    pub fn value<T: core::ContextDatum>(&self, key: &core::ContextKey<T>) -> Result<Option<T>> {
        self.check()?;
        if self.frame == 0 {
            return Ok(None);
        }
        self.host
            .kernel()
            .context_value(self.root, self.frame, &key.contract())?
            .map(T::from_value)
            .transpose()
    }

    /// Reject an expired root, or a completed invocation or one its caller cancelled.
    pub fn check(&self) -> Result<()> {
        self.host.kernel().check_root(self.root)?;
        if let Some(call) = &self.call {
            self.host.kernel().check_call(call.id)?;
        }
        Ok(())
    }

    /// Signal root cancellation without releasing physical ownership.
    pub fn cancel(&self) -> Result<()> {
        self.host.kernel().cancel_root(self.root)?;
        self.host.cancel_context(self.root);
        self.host.flush();
        Ok(())
    }

    /// Read the identity retained by this root.
    pub fn principal(&self) -> Result<String> {
        self.host.kernel().root_principal(self.root)
    }

    /// The entered invocation that issues calls made from this context, if any.
    pub(crate) fn issuer(&self) -> Option<core::Id> {
        self.call.as_ref().map(|call| call.id)
    }

    /// Require an admitted call context for owner operations.
    pub fn invocation(&self) -> Result<core::Invocation> {
        self.call
            .clone()
            .ok_or_else(|| Error::invalid(Reason::Context, "an entered invocation is required"))
    }
}

impl Context {
    /// Observe cancellation of this operation's root, including host shutdown. An entered
    /// invocation's signal also fires when its caller cancels that invocation (or one it
    /// descends from); that never cancels the root or sibling calls.
    pub fn signal(&self) -> Cancellation {
        match &self.call {
            Some(call) => self.host.call_signal(self.root, call.id),
            None => self.host.context_signal(self.root),
        }
    }
}

impl Host {
    /// One shared signal per entered invocation, created when first observed: cancelled at once
    /// if its root or the invocation itself already was.
    fn call_signal(&self, root: core::Id, call: core::Id) -> Cancellation {
        let mut signals = self.0.call_signals.borrow_mut();
        signals.retain(|_, (_, signal)| signal.strong_count() != 0);
        if let Some(signal) = signals.get(&call).and_then(|(_, signal)| signal.upgrade()) {
            return Cancellation(signal);
        }
        let signal = Cancellation::new("root");
        if self.kernel().check_root(root).is_err() {
            signal.cancel();
        } else if self.kernel().call_cancelled(call) {
            signal.cancel_because("call");
        }
        signals.insert(call, (root, Rc::downgrade(&signal.0)));
        signal
    }

    /// Mirror a kernel call cancellation into that invocation's signal, if it is observed.
    pub fn cancel_call_signal(&self, call: core::Id) {
        let signal = self
            .0
            .call_signals
            .borrow()
            .get(&call)
            .and_then(|(_, signal)| signal.upgrade());
        if let Some(signal) = signal {
            Cancellation(signal).cancel_because("call");
        }
    }

    fn context_signal(&self, root: core::Id) -> Cancellation {
        let mut signals = self.0.context_signals.borrow_mut();
        signals.retain(|_, signal| signal.strong_count() != 0);
        if let Some(signal) = signals.get(&root).and_then(std::rc::Weak::upgrade) {
            return Cancellation(signal);
        }
        let signal = Cancellation::new("root");
        if self.kernel().check_root(root).is_err() {
            signal.cancel();
        }
        signals.insert(root, Rc::downgrade(&signal.0));
        signal
    }

    /// Private lifecycle notification for waiting acquisition, never exposed as Context::signal.
    /// Normal root completion must not cancel admitted or streaming physical work.
    fn root_end_signal(&self, root: core::Id) -> Cancellation {
        let mut signals = self.0.root_end_signals.borrow_mut();
        signals.retain(|_, signal| signal.strong_count() != 0);
        if let Some(signal) = signals.get(&root).and_then(std::rc::Weak::upgrade) {
            return Cancellation(signal);
        }
        let signal = Cancellation::new("root_end");
        if self.kernel().check_root(root).is_err() {
            signal.cancel();
        }
        signals.insert(root, Rc::downgrade(&signal.0));
        signal
    }

    /// Mirror a shared kernel cancellation action into native context signals, and cancel the
    /// stream ends delivered to this root at once.
    pub fn cancel_context(&self, root: core::Id) {
        if let Some(signal) = self
            .0
            .context_signals
            .borrow()
            .get(&root)
            .and_then(std::rc::Weak::upgrade)
        {
            Cancellation(signal).cancel();
        }
        // Signals of the root's entered invocations observe the root too.
        let calls: Vec<_> = self
            .0
            .call_signals
            .borrow()
            .values()
            .filter(|(owner, _)| *owner == root)
            .filter_map(|(_, signal)| signal.upgrade())
            .collect();
        for signal in calls {
            Cancellation(signal).cancel_because("root");
        }
        for stream in self.root_streams(root, false) {
            stream.cancel_root();
        }
    }

    /// Bind a delivered stream end to the root that received it.
    pub(crate) fn bind_root_stream(
        &self,
        root: core::Id,
        stream: std::rc::Weak<dyn crate::portable::RootStream>,
    ) -> Result<()> {
        self.kernel().check_root(root)?;
        let mut claims = self.0.root_references.borrow_mut();
        let streams = &mut claims.entry(root).or_default().streams;
        streams.retain(|stream| stream.strong_count() != 0);
        streams.push(stream);
        Ok(())
    }

    /// The live stream ends of a root, taken out of the registry when the root ends. Collected
    /// first, so a stream's close never runs while the registry is borrowed.
    fn root_streams(&self, root: core::Id, take: bool) -> Vec<Rc<dyn crate::portable::RootStream>> {
        let mut claims = self.0.root_references.borrow_mut();
        let Some(claims) = claims.get_mut(&root) else {
            return Vec::new();
        };
        let streams = if take {
            std::mem::take(&mut claims.streams)
        } else {
            claims.streams.clone()
        };
        streams.iter().filter_map(std::rc::Weak::upgrade).collect()
    }
}

impl Context {
    /// Acquire a reference whose claim is revoked when this operation's root ends.
    pub fn get(
        &self,
        service: &str,
        key: Vec<String>,
        options: crate::AcquisitionOptions,
    ) -> Result<Rc<Reference>> {
        self.check()?;
        let reference =
            Rc::new(
                self.host
                    .get_with_options(service, key, &self.principal()?, options)?,
            );
        self.host
            .0
            .root_references
            .borrow_mut()
            .entry(self.root)
            .or_default()
            .references
            .push(reference.clone());
        Ok(reference)
    }
}

impl Context {
    /// Acquire under this root after the exact previous instance physically closes.
    /// Cancellation or root expiry ends only this wait; it never interrupts the old cleanup.
    /// The principal, context and admission requirements are checked again on every attempt.
    /// Use [`Reference::ready`] separately to await the new instance's activation.
    pub async fn get_wait(
        &self,
        service: &str,
        key: Vec<String>,
        options: crate::AcquisitionOptions,
    ) -> Result<Rc<Reference>> {
        let signal = self.signal();
        loop {
            match self.get(service, key.clone(), options.clone()) {
                Ok(reference) => return Ok(reference),
                Err(error) => {
                    let wait = self
                        .host
                        .closing(service, &key, &self.principal()?, error)?;
                    let root_signal = self.host.root_end_signal(self.root);
                    let cancelled = std::pin::pin!(async {
                        let call = std::pin::pin!(signal.cancelled());
                        let root = std::pin::pin!(root_signal.cancelled());
                        futures::future::select(call, root).await;
                    });
                    let wait = std::pin::pin!(wait);
                    if let futures::future::Either::Left(_) =
                        futures::future::select(cancelled, wait).await
                    {
                        self.check()?;
                        signal.check()?;
                        root_signal.check()?;
                    }
                }
            }
        }
    }

    /// Access the native environment shared by the currently entered orb.
    pub fn orb<T: 'static>(&self) -> Result<Rc<T>> {
        self.check()?;
        let call = self.invocation()?;
        self.host
            .0
            .orb_values
            .borrow()
            .get(&call.orb)
            .cloned()
            .ok_or_else(|| Error::invalid(Reason::Orb, "orb environment unavailable"))?
            .downcast::<T>()
            .map_err(|_| type_error())
    }

    /// Access a host-owned resource during an active operation.
    pub fn resource<T: 'static>(&self, name: &str) -> Result<Resource<T>> {
        self.check()?;
        self.host.resource(name)
    }
}

impl Host {
    /// Revoke native claims acquired inside a root whose logical wait has ended.
    /// Shared drivers call this for roots they issue themselves.
    /// Stream ends delivered to the root are closed first, so their producers stop before the
    /// root's claims are revoked; a retained end then refuses every read.
    pub fn end_context(&self, root: core::Id) {
        let ended = self
            .0
            .root_end_signals
            .borrow_mut()
            .remove(&root)
            .and_then(|signal| signal.upgrade());
        if let Some(ended) = ended {
            Cancellation(ended).cancel();
        }
        for stream in self.root_streams(root, true) {
            stream.end_root();
        }
        let references = self
            .0
            .root_references
            .borrow_mut()
            .remove(&root)
            .unwrap_or_default()
            .references;
        for reference in references {
            if let Err(error) = reference.release() {
                self.0.errors.borrow_mut().push(error);
            }
        }
    }
}
