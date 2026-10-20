use super::*;
use crate::{
    host::reload::{DeclarationSet, InstalledVersion},
    invocation::retain_activity,
};
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Staging,
    Installed,
    Paused,
    Stopped,
}
struct ListenerRun {
    host: Weak<HostInner>,
    owner: Weak<ObjectRecord>,
    declaration: RefCell<Rc<ListenerDeclaration>>,
    queue: RefCell<VecDeque<Box<dyn Any>>>,
    detach: RefCell<Option<Detach>>,
    mode: Cell<Mode>,
    running: Cell<bool>,
    failure: RefCell<Option<Rc<RuntimeError>>>,
    ready: Cell<bool>,
    current: RefCell<Option<CancellationToken>>,
}
impl ListenerRun {
    fn receive(self: &Rc<Self>, event: Box<dyn Any>) {
        if self.mode.get() == Mode::Stopped {
            return;
        }
        let declaration = self.declaration.borrow().clone();
        let mut retired = VecDeque::new();
        if declaration.state {
            retired = std::mem::take(&mut *self.queue.borrow_mut());
        }
        let full = self.queue.borrow().len() >= declaration.options.max_pending;
        if full {
            let error = RuntimeError::unavailable(
                "listener_backlog",
                "listener pending event limit reached",
            );
            *self.failure.borrow_mut() = Some(Rc::new(error.clone()));
            let installed =
                self.ready.get() || matches!(self.mode.get(), Mode::Installed | Mode::Paused);
            self.mode.set(Mode::Stopped);
            if installed && let Some(host) = self.host.upgrade() {
                host.errors.borrow_mut().push(error);
            }
            let detach = self.detach.borrow_mut().take();
            let queued = std::mem::take(&mut *self.queue.borrow_mut());
            if let Some(host) = self.host.upgrade() {
                host.driver.spawn(
                    async move {
                        drop(event);
                        drop(queued);
                        if let Some(detach) = detach {
                            detach();
                        }
                    }
                    .boxed_local(),
                );
            }
            return;
        }
        self.queue.borrow_mut().push_back(event);
        drop(retired);
        self.schedule();
    }
    fn schedule(self: &Rc<Self>) {
        if self.mode.get() != Mode::Installed || self.running.replace(true) {
            return;
        }
        let Some(host) = self.host.upgrade() else {
            self.running.set(false);
            return;
        };
        let this = self.clone();
        host.driver.spawn(
            async move {
                while this.mode.get() == Mode::Installed {
                    let event = this.queue.borrow_mut().pop_front();
                    let Some(event) = event else {
                        break;
                    };
                    let result = this.deliver(event).await;
                    if let Err(error) = result
                        && let Some(host) = this.host.upgrade()
                    {
                        host.errors.borrow_mut().push(error);
                    }
                }
                this.running.set(false);
            }
            .boxed_local(),
        );
    }
    async fn deliver(self: &Rc<Self>, event: Box<dyn Any>) -> Result<()> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("activity_owner", "listener owner ended"))?;
        let admission = ActivityAdmission::new();
        let (root, root_guard) = host.activity_root(&owner, &admission)?;
        let (context, logical) = host.enter_listener(&owner, &root)?;
        *self.current.borrow_mut() = Some(context.cancellation().clone());
        let handler = self.declaration.borrow().handler.clone();
        let result = handler(event, context).await;
        let token = self.current.borrow_mut().take();
        drop(token);
        drop(logical);
        drop(root_guard);
        result
    }
    fn pause(&self, cause: RuntimeError) {
        if self.mode.get() != Mode::Stopped {
            self.mode.set(Mode::Paused);
        }
        let token = self.current.borrow().clone();
        if let Some(token) = token {
            token.cancel(cause);
        }
    }
    fn stop(&self, cause: RuntimeError) -> ActivityActions {
        self.mode.set(Mode::Stopped);
        let token = self.current.borrow().clone();
        let detach = self.detach.borrow_mut().take();
        let queued = std::mem::take(&mut *self.queue.borrow_mut());
        ActivityActions {
            actions: vec![Box::new(move || {
                if let Some(token) = token {
                    token.cancel(cause);
                }
                if let Some(detach) = detach {
                    detach();
                }
                drop(queued);
            })],
        }
    }
}
struct TaskRun {
    host: Weak<HostInner>,
    owner: Weak<ObjectRecord>,
    declarations: Weak<DeclarationSet>,
    start: Task,
    admission: RefCell<Rc<ActivityAdmission>>,
    running: Cell<bool>,
    restart: Cell<bool>,
    retired: Cell<bool>,
    physical: RefCell<Option<crate::InvocationReceipt>>,
    restart_waiting: Cell<bool>,
}
impl TaskRun {
    fn launch(self: &Rc<Self>) {
        if self.running.get() || self.retired.get() {
            return;
        }
        let Some(host) = self.host.upgrade() else {
            return;
        };
        if host.is_closing() {
            return;
        }
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let previous = self.physical.borrow().clone();
        if let Some(receipt) = previous.filter(|receipt| !receipt.is_settled()) {
            if !self.restart_waiting.replace(true) {
                let weak = Rc::downgrade(self);
                host.driver.spawn(
                    async move {
                        receipt.settled().await;
                        if let Some(task) = weak.upgrade() {
                            task.restart_waiting.set(false);
                            if task.restart.get() && !task.retired.get() {
                                task.launch();
                            }
                        }
                    }
                    .boxed_local(),
                );
            }
            return;
        }
        if owner.gate.borrow().is_some() {
            return;
        }
        let Some(declarations) = self.declarations.upgrade() else {
            return;
        };
        let selected = owner
            .installed
            .borrow()
            .get(&owner.current_epoch.get())
            .cloned();
        let Some(installed) =
            selected.filter(|installed| Rc::ptr_eq(&installed.declarations, &declarations))
        else {
            return;
        };
        let admission = ActivityAdmission::new();
        let old = self.admission.replace(admission.clone());
        drop(old);
        let physical = match retain_activity(&host, &owner, installed, admission.signal()) {
            Ok(lease) => lease,
            Err(error) => {
                host.errors.borrow_mut().push(error);
                return;
            }
        };
        physical.attach_activity(&admission);
        let receipt = physical
            .activity_receipt()
            .expect("retained task entry owns its receipt");
        let previous = self.physical.replace(Some(receipt));
        drop(previous);
        self.running.set(true);
        self.restart.set(false);
        let scope = TaskScope {
            host: self.host.clone(),
            object: self.owner.clone(),
            admission: Rc::downgrade(&admission),
            declarations: self.declarations.clone(),
        };
        let start = self.start.clone();
        let this = self.clone();
        host.driver.spawn(
            async move {
                let result = start(scope).await;
                admission.stop(RuntimeError::aborted(
                    "activity_stopped",
                    "task callback settled",
                ));
                this.running.set(false);
                drop(physical);
                if let Err(error) = result
                    && let Some(host) = this.host.upgrade()
                {
                    host.errors.borrow_mut().push(error);
                }
                if this.restart.get() && !this.retired.get() {
                    this.launch();
                }
            }
            .boxed_local(),
        );
    }
    fn pause(&self, cause: RuntimeError) {
        self.restart.set(false);
        let admission = self.admission.borrow().clone();
        admission.stop(cause);
    }
    fn resume(self: &Rc<Self>) {
        if !self.retired.get() {
            self.restart.set(true);
            self.launch();
        }
    }
    fn stop(&self, cause: RuntimeError) {
        self.retired.set(true);
        self.restart.set(false);
        let admission = self.admission.borrow().clone();
        admission.stop(cause);
    }
}

pub(crate) struct ActivityRunSet {
    listeners: BTreeMap<String, Rc<ListenerRun>>,
    tasks: Vec<Rc<TaskRun>>,
}
impl ActivityRunSet {
    pub(crate) fn pause(&self, cause: RuntimeError) {
        for listener in self.listeners.values() {
            listener.pause(cause.clone());
        }
        self.pause_tasks(cause);
    }
    /// Revokes task root admission before signalling cooperative stop.
    pub(crate) fn pause_tasks(&self, cause: RuntimeError) {
        for task in &self.tasks {
            task.pause(cause.clone());
        }
    }
    /// Captures actual task entry receipts, including independent roots sharing each entry.
    /// Listener and ordinary method entries are deliberately excluded.
    pub(crate) fn task_receipts_settled(&self) -> LocalBoxFuture<'static, ()> {
        let receipts = self
            .tasks
            .iter()
            .filter_map(|task| task.physical.borrow().clone())
            .collect::<Vec<_>>();
        async move {
            futures::future::join_all(receipts.into_iter().map(|receipt| async move {
                receipt.settled().await;
            }))
            .await;
        }
        .boxed_local()
    }
    /// Called only after resource restoration. Lingering callbacks prevent duplicate restart.
    pub(crate) fn resume_after_abort(&self) -> ActivityActions {
        let listeners = self.listeners.values().cloned().collect::<Vec<_>>();
        let tasks = self.tasks.clone();
        ActivityActions {
            actions: vec![Box::new(move || {
                for listener in listeners {
                    if listener.mode.get() != Mode::Stopped {
                        listener.mode.set(Mode::Installed);
                        listener.schedule();
                    }
                }
                for task in tasks {
                    task.resume();
                }
            })],
        }
    }
    pub(crate) fn stop(&self, cause: RuntimeError) -> ActivityActions {
        self.retire(&[], cause)
    }
    pub(crate) fn retire(
        &self,
        retained: &[Rc<ActivityRunSet>],
        cause: RuntimeError,
    ) -> ActivityActions {
        let mut actions = ActivityActions::default();
        for listener in self.listeners.values() {
            if !retained.iter().any(|set| {
                set.listeners
                    .values()
                    .any(|other| Rc::ptr_eq(listener, other))
            }) {
                actions.extend(listener.stop(cause.clone()));
            }
        }
        let tasks = self.tasks.clone();
        actions.actions.push(Box::new(move || {
            for task in tasks {
                task.stop(cause.clone());
            }
        }));
        actions
    }
    pub(crate) fn failure_checkpoint(&self) -> ActivityFailureCheckpoint {
        ActivityFailureCheckpoint {
            listeners: self
                .listeners
                .values()
                .map(|run| (Rc::downgrade(run), run.failure.borrow().clone()))
                .collect(),
        }
    }
}
/// A transaction-local observation; historical stream failures are never cleared.
pub(crate) struct ActivityFailureCheckpoint {
    listeners: Vec<(Weak<ListenerRun>, Option<Rc<RuntimeError>>)>,
}
impl ActivityFailureCheckpoint {
    pub(crate) fn check(&self) -> Result<()> {
        for (run, baseline) in &self.listeners {
            if let Some(run) = run.upgrade()
                && let Some(error) = run.failure.borrow().clone()
                && baseline.as_ref().is_none_or(|old| !Rc::ptr_eq(old, &error))
            {
                return Err(error.as_ref().clone());
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct ActivityActions {
    actions: Vec<Box<dyn FnOnce()>>,
}
impl ActivityActions {
    pub(crate) fn start(self) {
        for action in self.actions {
            action();
        }
    }
    pub(crate) fn extend(&mut self, other: Self) {
        self.actions.extend(other.actions);
    }
}
pub(crate) struct PreparedActivities {
    host: Weak<HostInner>,
    owner: Weak<ObjectRecord>,
    installed: Weak<InstalledVersion>,
    listeners: BTreeMap<String, (Rc<ListenerRun>, Rc<ListenerDeclaration>, bool)>,
    tasks: Vec<Task>,
    previous: Option<Rc<ActivityRunSet>>,
    adopted: bool,
    ready: bool,
}
impl ActivityDeclarations {
    pub(crate) fn clear(&self) -> ActivityActions {
        let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        ActivityActions {
            actions: vec![Box::new(move || {
                drop(listeners);
                drop(tasks);
            })],
        }
    }
    pub(crate) fn prepare(
        self: &Rc<Self>,
        host: &Rc<HostInner>,
        owner: &Rc<ObjectRecord>,
        installed: Rc<InstalledVersion>,
        previous: Option<&Rc<ActivityRunSet>>,
    ) -> Result<PreparedActivities> {
        let declarations = self
            .listeners
            .borrow()
            .iter()
            .map(|(name, decl)| (name.clone(), decl.clone()))
            .collect::<Vec<_>>();
        let mut prepared = PreparedActivities {
            host: Rc::downgrade(host),
            owner: Rc::downgrade(owner),
            installed: Rc::downgrade(&installed),
            listeners: BTreeMap::new(),
            tasks: self.tasks.borrow().clone(),
            previous: previous.cloned(),
            adopted: false,
            ready: false,
        };
        for (name, declaration) in declarations {
            let reuse = previous
                .and_then(|old| old.listeners.get(&name))
                .filter(|old| {
                    old.mode.get() != Mode::Stopped
                        && (declaration.same)(old.declaration.borrow().identity.as_ref())
                })
                .cloned();
            let reused = reuse.is_some();
            let run = reuse.unwrap_or_else(|| {
                Rc::new(ListenerRun {
                    host: Rc::downgrade(host),
                    owner: Rc::downgrade(owner),
                    declaration: RefCell::new(declaration.clone()),
                    queue: RefCell::default(),
                    detach: RefCell::default(),
                    mode: Cell::new(Mode::Staging),
                    running: Cell::new(false),
                    failure: RefCell::default(),
                    ready: Cell::new(false),
                    current: RefCell::default(),
                })
            });
            if !reused {
                let weak = Rc::downgrade(&run);
                let subscribe = declaration.subscribe.clone();
                match subscribe(
                    Rc::new(move |event| {
                        if let Some(run) = weak.upgrade() {
                            run.receive(event);
                        }
                    }),
                    Some(&installed.declarations.state),
                ) {
                    Ok(detach) => *run.detach.borrow_mut() = Some(detach),
                    Err(error) => {
                        prepared.listeners.insert(name, (run, declaration, false));
                        prepared.abort().start();
                        return Err(error);
                    }
                }
                if let Some(error) = run.failure.borrow().clone() {
                    prepared
                        .listeners
                        .insert(name, (run.clone(), declaration, false));
                    prepared.abort().start();
                    return Err(error.as_ref().clone());
                }
            }
            prepared.listeners.insert(name, (run, declaration, reused));
        }
        Ok(prepared)
    }
}
impl PreparedActivities {
    pub(crate) fn preflight(&self) -> Result<()> {
        if self.adopted {
            return Err(RuntimeError::invalid(
                "activity_candidate",
                "activities already adopted",
            ));
        }
        if !self.ready {
            for (run, _, _) in self.listeners.values() {
                if let Some(error) = run.failure.borrow().clone() {
                    return Err(error.as_ref().clone());
                }
            }
        }
        Ok(())
    }
    /// Freezes the listener part of an irrevocable Ready vote. Later overflow is
    /// installed-like: stop, detach and report; never invalidate this candidate.
    pub(crate) fn seal_ready(&mut self) -> Result<()> {
        self.preflight()?;
        self.ready = true;
        for (run, _, _) in self.listeners.values() {
            run.ready.set(true);
        }
        Ok(())
    }

    pub(crate) fn adopt(&mut self) -> (Rc<ActivityRunSet>, ActivityActions) {
        self.adopted = true;
        let mut retired = Vec::new();
        let mut listeners = BTreeMap::new();
        for (name, (run, declaration, _)) in &self.listeners {
            retired.push(run.declaration.replace(declaration.clone()));
            listeners.insert(name.clone(), run.clone());
        }
        let installed = self
            .installed
            .upgrade()
            .expect("prepared installed record retained by coordinator");
        let tasks = self
            .tasks
            .iter()
            .map(|start| {
                Rc::new(TaskRun {
                    host: self.host.clone(),
                    owner: self.owner.clone(),
                    declarations: Rc::downgrade(&installed.declarations),
                    start: start.clone(),
                    admission: RefCell::new(ActivityAdmission::new()),
                    running: Cell::new(false),
                    restart: Cell::new(false),
                    retired: Cell::new(false),
                    physical: RefCell::default(),
                    restart_waiting: Cell::new(false),
                })
            })
            .collect::<Vec<_>>();
        let runs = Rc::new(ActivityRunSet { listeners, tasks });
        let launch = runs.clone();
        let mut actions = ActivityActions::default();
        if let Some(previous) = &self.previous {
            actions.extend(previous.retire(
                std::slice::from_ref(&runs),
                RuntimeError::aborted("replaced", "activity declarations replaced"),
            ));
        }
        actions.actions.push(Box::new(move || {
            drop(retired);
            for listener in launch.listeners.values() {
                if listener.mode.get() != Mode::Stopped {
                    listener.mode.set(Mode::Installed);
                    listener.schedule();
                }
            }
            for task in &launch.tasks {
                task.launch();
            }
        }));
        (runs, actions)
    }
    pub(crate) fn abort(self) -> ActivityActions {
        let mut actions = ActivityActions::default();
        for (run, _, reused) in self.listeners.into_values() {
            if !reused {
                actions.extend(run.stop(RuntimeError::aborted(
                    "prepare",
                    "activity preparation aborted",
                )));
            }
        }
        actions
    }
}
