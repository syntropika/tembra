use crate::{callbacks::Callback, handles::Entry};
use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};
use napi::{
    Env,
    bindgen_prelude::{Function, FunctionRef},
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
    thread::ThreadId,
};
use tembra_core::{Driver, Host, HostOptions, Result, RuntimeError};
use tembra_native::{TokioDriver, random_incarnation};

thread_local! { static SCHEDULER: RefCell<Weak<Scheduler>> = const { RefCell::new(Weak::new()) }; }

pub(crate) struct Scheduler {
    pub driver: Rc<TokioDriver>,
    pub runtime: tokio::runtime::Runtime,
    polling: Cell<bool>,
}
impl Scheduler {
    fn get() -> napi::Result<Rc<Self>> {
        if let Some(scheduler) = SCHEDULER.with(|slot| slot.borrow().upgrade()) {
            return Ok(scheduler);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let result = Rc::new(Self {
            runtime,
            driver: TokioDriver::new(),
            polling: Cell::new(false),
        });
        SCHEDULER.with(|slot| *slot.borrow_mut() = Rc::downgrade(&result));
        Ok(result)
    }
    pub fn poll(&self) -> napi::Result<()> {
        if self.polling.replace(true) {
            return Err(napi::Error::from_reason("recursive scheduler drive"));
        }
        struct Reset<'a>(&'a Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.polling);
        self.runtime.block_on(
            self.driver
                .run_until(async { tokio::task::yield_now().await }),
        );
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Handoffs {
    pub callbacks: Vec<String>,
    pub values: Vec<String>,
}

pub(crate) struct Session {
    pub handoffs: RefCell<Vec<Handoffs>>,
    pub host: Host,
    pub loader: Rc<crate::loader::Environment>,
    pub generation: RefCell<Option<Rc<crate::loader::Generation>>>,
    pub generations: RefCell<BTreeMap<String, Weak<crate::loader::Generation>>>,
    pub released_generations: RefCell<std::collections::VecDeque<Value>>,
    provenance: RefCell<BTreeMap<String, Weak<crate::loader::Generation>>>,
    pub scheduler: Rc<Scheduler>,
    env: Env,
    thread: ThreadId,
    dispatcher: RefCell<Option<FunctionRef<String, String>>>,
    pub entries: RefCell<BTreeMap<String, Entry>>,
    pub events: RefCell<Vec<Value>>,
    pub callback_errors: RefCell<Vec<RuntimeError>>,
    pending: RefCell<BTreeMap<String, oneshot::Sender<Result<Value>>>>,
    pub jobs: Cell<usize>,
    pub callback_jobs: Cell<usize>,
    pub closing: Cell<bool>,
    pub hook_managers: RefCell<BTreeMap<String, tembra_hooks::HookManager>>,
    pub hooks: RefCell<BTreeMap<String, tembra_hooks::portable::PortableHook>>,
    pub hook_signatures: RefCell<BTreeMap<String, Rc<dyn crate::hooks::Signature>>>,
    pub opaque: RefCell<std::collections::BTreeSet<String>>,
    pub event_sources: RefCell<BTreeMap<String, Weak<crate::activity::EventBinding>>>,
}
impl Session {
    pub fn new(
        env: Env,
        name: String,
        options: &str,
        dispatch: Function<String, String>,
    ) -> napi::Result<Rc<Self>> {
        let scheduler = Scheduler::get()?;
        let options: Value =
            serde_json::from_str(options).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        if options.as_object().is_none_or(|o| !o.is_empty()) {
            return Err(napi::Error::from_reason("unsupported native Host options"));
        }
        Ok(Rc::new(Self {
            handoffs: RefCell::default(),
            host: Host::new(
                HostOptions::new(name, json!(null)),
                scheduler.driver.clone(),
            ),
            scheduler,
            loader: crate::loader::Environment::get(env)?,
            generation: RefCell::default(),
            generations: RefCell::default(),
            released_generations: RefCell::default(),
            provenance: RefCell::default(),
            env,
            thread: std::thread::current().id(),
            dispatcher: RefCell::new(Some(dispatch.create_ref()?)),
            entries: RefCell::default(),
            events: RefCell::default(),
            callback_errors: RefCell::default(),
            pending: RefCell::default(),
            event_sources: RefCell::default(),
            hooks: RefCell::default(),
            hook_managers: RefCell::default(),
            hook_signatures: RefCell::default(),
            opaque: RefCell::default(),
            jobs: Cell::new(0),
            callback_jobs: Cell::new(0),
            closing: Cell::new(false),
        }))
    }
    pub fn detach_dispatcher(&self) {
        // Break the NodeBridge -> FunctionRef -> JavaScript dispatcher -> NodeBridge cycle.
        let dispatcher = self.dispatcher.borrow_mut().take();
        drop(dispatcher);
    }
    pub fn check_thread(&self) -> napi::Result<()> {
        if self.thread != std::thread::current().id() {
            return Err(napi::Error::from_reason(
                "Host belongs to another JavaScript thread",
            ));
        }
        Ok(())
    }
    pub fn issue(&self, entry: Entry) -> Result<Value> {
        let kind = entry.kind();
        let id = random_incarnation()?;
        if let Some(generation) = self.generation.borrow().as_ref() {
            self.provenance
                .borrow_mut()
                .insert(id.clone(), Rc::downgrade(generation));
        }
        self.entries.borrow_mut().insert(id.clone(), entry);
        Ok(json!({"$handle": id, "$kind": kind}))
    }
    pub fn entry(&self, id: &str) -> Result<Entry> {
        self.entries.borrow().get(id).cloned().ok_or_else(|| {
            RuntimeError::invalid("foreign_handle", "handle is foreign, freed or expired")
        })
    }
    pub fn remove(&self, id: &str) {
        self.provenance.borrow_mut().remove(id);
        self.hook_managers.borrow_mut().remove(id);
        let removed = self.entries.borrow_mut().remove(id);
        drop(removed);
    }
    pub fn callback(self: &Rc<Self>, value: &Value) -> Result<Rc<Callback>> {
        let id = value
            .get("$callback")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::invalid("callback", "expected issued callback"))?;
        if let Some(handoff) = self.handoffs.borrow_mut().last_mut() {
            handoff.callbacks.push(id.to_owned());
        }
        let generation = self.generation.borrow().clone();
        if let Some(generation) = &generation {
            generation.callbacks.set(generation.callbacks.get() + 1);
        }
        Ok(Rc::new(Callback {
            generation,
            session: Rc::downgrade(self),
            id: id.into(),
        }))
    }
    pub fn enter_generation(
        &self,
        generation: Option<Rc<crate::loader::Generation>>,
    ) -> crate::loader::GenerationGuard<'_> {
        crate::loader::GenerationGuard::new(&self.generation, generation)
    }
    pub fn enter_handle(&self, id: &str) -> crate::loader::GenerationGuard<'_> {
        let generation = self
            .provenance
            .borrow()
            .get(id)
            .and_then(Weak::upgrade)
            .or_else(|| self.generation.borrow().clone());
        self.enter_generation(generation)
    }
    pub fn dispatch(&self, request: Value) -> Result<Value> {
        let dispatcher = self.dispatcher.borrow();
        let callback = dispatcher
            .as_ref()
            .ok_or_else(|| RuntimeError::unavailable("closed", "callback dispatcher ended"))?
            .borrow_back(&self.env)
            .map_err(js_error)?;
        let text = callback.call(request.to_string()).map_err(js_error)?;
        let envelope: Value = serde_json::from_str(&text)
            .map_err(|e| RuntimeError::invalid("javascript", e.to_string()))?;
        unwrap_result(envelope)
    }
    pub fn start(self: &Rc<Self>, future: LocalBoxFuture<'static, Result<Value>>) -> Result<Value> {
        let id = random_incarnation()?;
        let session = self.clone();
        let job = id.clone();
        self.jobs.set(self.jobs.get() + 1);
        self.scheduler.driver.spawn(
            async move {
                let result = future.await;
                session.jobs.set(session.jobs.get() - 1);
                session.events.borrow_mut().push(match result {
                    Ok(value) => json!({"job": job, "ok": value}),
                    Err(error) => json!({"job": job, "error": error}),
                });
            }
            .boxed_local(),
        );
        Ok(json!({"$job": id}))
    }
    pub fn invoke(
        self: &Rc<Self>,
        callback: Rc<Callback>,
        args: Vec<Value>,
        retain: bool,
        discard: bool,
        portable: u8,
    ) -> LocalBoxFuture<'static, Result<Value>> {
        let session = self.clone();
        async move {
            if session.callback_jobs.get() >= 128 { return Err(RuntimeError::unavailable("jobs_limit", "native callback capacity exhausted")); }
            let job = random_incarnation()?;
            let (send, receive) = oneshot::channel();
            session.pending.borrow_mut().insert(job.clone(), send);
            session.callback_jobs.set(session.callback_jobs.get() + 1);
            let _physical = crate::loader::PhysicalLease::new(callback.generation.clone());
            let guard = session.enter_generation(callback.generation.clone());
            let called = session.dispatch(json!({"callback": callback.id, "args": args, "job": job, "retain": retain, "discard": discard,"portable":portable}));
            drop(guard);
            if let Err(error) = called {
                session.pending.borrow_mut().remove(&job);
                session.callback_jobs.set(session.callback_jobs.get() - 1);
                return Err(error);
            }
            let result = receive.await.map_err(|_| RuntimeError::unavailable("javascript", "callback observation ended"));
            let mut result = result?;
            if retain && let Some(generation) = &callback.generation && let Ok(Value::Object(fields)) = &mut result {
                fields.insert("$generation".into(), json!(generation.id));
            }
            session.callback_jobs.set(session.callback_jobs.get() - 1);
            result
        }.boxed_local()
    }
    pub fn complete(&self, job: &str, envelope: Value) -> Result<()> {
        let send = self.pending.borrow_mut().remove(job).ok_or_else(|| {
            RuntimeError::invalid("foreign_job", "callback was not issued by this Host")
        })?;
        let _ = send.send(unwrap_result(envelope));
        Ok(())
    }
}
fn unwrap_result(envelope: Value) -> Result<Value> {
    if let Some(error) = envelope.get("error") {
        return Err(serde_json::from_value(error.clone())
            .unwrap_or_else(|_| RuntimeError::invalid("javascript", error.to_string())));
    }
    envelope
        .get("ok")
        .cloned()
        .ok_or_else(|| RuntimeError::invalid("javascript", "missing callback result"))
}
fn js_error(error: napi::Error) -> RuntimeError {
    RuntimeError::invalid("javascript", error.to_string())
}
