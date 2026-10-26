//! Owner-bound hook observation uses the shared kernel admission and managed executor.

use super::context::RootGuard;
use super::*;

impl<A: 'static> crate::HookHandle<A, ()> {
    /// Await this hook once on behalf of its still-owned physical source invocation.
    ///
    /// # Guarantees
    ///
    /// The source must belong to this host and declaring orb. An object-bound handle requires
    /// the same source object. An orb-bound handle also retains the source object's ownership.
    /// The first successful admission consumes the source's single observation admission across
    /// its hook handles, including an empty plan or a failing participant. Refusal does not
    /// consume admission. An observer or its dependent calls cannot admit another observation.
    ///
    /// Participants use the source's principal and exact versioned plan under a fresh ordinary
    /// root with an empty metadata frame. Supply correlation data in `input`; original contexts,
    /// closed frames and storage access are not restored. Original cancellation remains effective.
    /// The physical owner must await this method before returning if observation must precede
    /// its settlement. A logically cancelled caller need not wait for physical completion.
    ///
    /// Dropping this wait leaves admitted work physically managed by the host. Exact source
    /// scope anchors survive its physical descendants; completed dependencies stop affecting
    /// observation. Cleanup cancels relevant work and waits for physical settlement. Cancellation
    /// is cooperative; an uncooperative participant can delay cleanup. Errors remain separate
    /// from the application's primary result and cannot undo effects or authorize replay.
    ///
    /// # Errors
    ///
    /// Returns `invalid/context` for foreign-host or root-only contexts, `gone/call` for a settled
    /// source, `forbidden/operation_owner` for a foreign owner, `forbidden/observation_source` for
    /// an observer or descendant, and `conflict/observation_admitted` for repeated admission.
    /// Missing/closing scopes, host shutdown and ordinary version/gate admission can also refuse
    /// observation. Refusal enters no participant and retains no new root or invocation.
    /// Once admitted, participant and executor errors propagate and admission remains consumed.
    /// Dependent calls additionally require their exact registered scopes and ancestors to be
    /// open: missing scopes return `gone/scope`, closing scopes return `conflict/scope_closed`.
    /// Ordinary calls retain their existing admission rules.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures::{FutureExt, executor::LocalPool};
    /// use tembra::{Code, Hook, Host, Orb, Service};
    ///
    /// let mut pool = LocalPool::new();
    /// let host = Host::new(pool.spawner());
    /// let observation = Hook::<String, ()>::series("finished");
    /// let bound = observation.clone();
    /// host.install(vec![Orb::new("example", 1)
    ///     .hook(observation.clone())
    ///     .contribute("example", &observation, 0, |ctx, payload| async move {
    ///         assert_eq!(ctx.principal()?, "alice");
    ///         assert_eq!(ctx.frame(), 0);
    ///         assert_eq!(payload, "Cancelled");
    ///         Ok(())
    ///     })
    ///     .service(Service::new("example", move |self_| self_.hook(&bound))
    ///         .method("run", |hook, ctx, (): ()| async move {
    ///             ctx.cancel()?;
    ///             hook.observe(&ctx, "Cancelled".into()).await?;
    ///             assert_eq!(ctx.check().unwrap_err().code, Code::Cancelled);
    ///             assert_eq!(hook.observe(&ctx, "again".into()).await
    ///                 .unwrap_err().reason, "observation_admitted");
    ///             Ok(())
    ///         }))])?;
    /// let reference = host.get("example", vec!["main".into()], "alice")?;
    /// pool.run_until(host.root("alice", move |ctx| async move {
    ///     reference.call::<_, ()>(&ctx, "run", ()).await
    /// }.boxed_local()))?;
    /// pool.run_until(host.close())?;
    /// assert_eq!(host.kernel().diagnostics().roots, 0);
    /// assert_eq!(host.kernel().diagnostics().calls, 0);
    /// # Ok::<(), tembra::Error>(())
    /// ```
    pub async fn observe(&self, source: &Context, input: A) -> Result<()> {
        if !self.host.same(&source.host) {
            return Err(Error::invalid(Reason::Context, "foreign host context"));
        }
        let source = source.invocation()?;
        self.host.ready_orb(self.orb).await?;
        let observation = self.host.kernel().begin_hook_observation(
            source.id,
            self.orb,
            self.operation,
            self.object,
            &self.hook.name,
        )?;
        let call = observation.invocation;
        let root = RootGuard {
            host: self.host.clone(),
            root: call.root,
        };
        let entered = self.host.context(call.root, Some(call.clone()))?;
        let (host, compose) = (self.host.clone(), self.hook.compose.clone());
        let receive = self.host.spawn_managed(call.id, async move {
            let _root = root;
            compose(host, entered, observation.plan, input).await
        })?;
        receive
            .await
            .map_err(|_| Error::invalid(Reason::Executor, "hook observation executor stopped"))?
    }
}
