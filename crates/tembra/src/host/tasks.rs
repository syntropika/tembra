//! Scope-owned background work remains pinned until actual callback settlement.
use super::context::RootGuard;
use super::*;

/// Authority for one declared in-memory background task.
#[derive(Clone)]
pub struct TaskScope {
    host: Host,
    task: core::Id,
    orb: core::Id,
    object: Option<core::Id>,
    operation: core::Id,
    principal: String,
    signal: Cancellation,
}

impl TaskScope {
    /// Observe task cancellation without treating it as physical completion.
    pub fn signal(&self) -> Cancellation {
        self.signal.clone()
    }

    /// Run a managed operation with the owner's identity and the current published code version.
    pub async fn root<T: 'static>(
        &self,
        operation: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>>,
    ) -> Result<T> {
        self.signal.check()?;
        let root = self.host.kernel().task_root(self.task, &self.principal)?;
        let _root = RootGuard {
            host: self.host.clone(),
            root,
        };
        let call =
            self.host
                .kernel()
                .begin_operation(self.orb, root, self.operation, self.object)?;
        let context = self.host.context(root, Some(call.clone()))?;
        let future = operation(context);
        let receive = self.host.spawn_managed(call.id, future)?;
        receive
            .await
            .map_err(|_| Error::invalid(Reason::Executor, "task operation executor stopped"))?
    }
}

impl Declaration {
    /// Declare background work to start after this scope is activated.
    pub fn task(
        &self,
        start: impl FnOnce(TaskScope) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.check_declaration()?;
        if self.orb == 0 {
            return Err(Error::invalid(
                Reason::Task,
                "host tasks require an orb owner",
            ));
        }
        self.draft.tasks.borrow_mut().push(Box::new(start));
        Ok(())
    }
}

impl Host {
    pub(super) fn start_scope(&self, scope: core::Id) -> Result<()> {
        let (owner, orb, starts) = {
            let scopes = self.0.scopes.borrow();
            let Some(scope) = scopes.get(&scope) else {
                return Ok(());
            };
            (scope.owner, scope.orb, scope.draft.tasks.take())
        };
        for start in starts {
            let task = self.kernel().begin_task(owner)?;
            let object = match owner {
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
            let signal = Cancellation::new("task");
            let authority = TaskScope {
                host: self.clone(),
                task,
                orb,
                object,
                operation: self.kernel().allocate_handle()?,
                principal,
                signal: signal.clone(),
            };
            self.0
                .task_signals
                .borrow_mut()
                .insert(task, signal.clone());
            self.0
                .scopes
                .borrow_mut()
                .get_mut(&scope)
                .expect("declared scope")
                .tasks
                .push(task);
            let guard = TaskGuard {
                host: self.clone(),
                task,
            };
            let host = self.clone();
            let receive = self.spawn(async move {
                let _guard = guard;
                if let Err(error) = start(authority).await
                    && !signal.is_cancelled()
                {
                    host.0.errors.borrow_mut().push(error);
                }
                Ok(())
            })?;
            drop(receive);
        }
        Ok(())
    }

    pub(super) fn stop_scope(&self, scope: core::Id) -> Result<()> {
        let tasks = self
            .0
            .scopes
            .borrow()
            .get(&scope)
            .map(|scope| scope.tasks.clone())
            .unwrap_or_default();
        for task in tasks {
            if self.0.task_signals.borrow().contains_key(&task) {
                self.kernel().stop_task(task)?;
                self.cancel_task(task);
            }
        }
        Ok(())
    }

    /// Mirror a shared kernel task-cancellation action into its native signal.
    pub fn cancel_task(&self, task: core::Id) {
        if let Some(signal) = self.0.task_signals.borrow().get(&task) {
            signal.cancel();
        }
    }

    pub(crate) fn spawn_managed<T: 'static>(
        &self,
        call: core::Id,
        operation: impl Future<Output = Result<T>> + 'static,
    ) -> Result<oneshot::Receiver<Result<T>>> {
        let guard = InvocationGuard {
            host: self.clone(),
            call,
        };
        self.spawn(async move {
            let _guard = guard;
            operation.await
        })
    }
}

struct TaskGuard {
    host: Host,
    task: core::Id,
}
impl Drop for TaskGuard {
    fn drop(&mut self) {
        if !self.host.0.external_actions.get() {
            self.host.tick();
        }
        if let Err(error) = self.host.kernel().finish_task(self.task) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.0.task_signals.borrow_mut().remove(&self.task);
        self.host.flush();
    }
}

struct InvocationGuard {
    host: Host,
    call: core::Id,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if !self.host.0.external_actions.get() {
            self.host.tick();
        }
        if let Err(error) = self.host.kernel().finish_call(self.call) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.flush();
    }
}
