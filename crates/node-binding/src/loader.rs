//! Node environment admission and managed callback ownership, independent of ESM cache lifetime.
use crate::session::Session;
use napi::Env;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    rc::{Rc, Weak},
    time::Instant,
};
use tembra_core::{FacetRecorder, FacetScope, FacetSignal, Result, RuntimeError};

const MAX_EVALUATIONS: usize = 32;
const MAX_SOURCE_BYTES: usize = 8_388_608;
const MAX_ARTIFACT_BYTES: usize = 1_048_576;
const MAX_SOURCE_GENERATIONS: usize = 256;
const MAX_TRANSFORMED_BYTES: usize = 67_108_864;
const MAX_SOURCE_MODULES: usize = 4096;
const MAX_SOURCE_MODULE_BYTES: usize = 2_097_152;
thread_local! {
    static ENVIRONMENTS: RefCell<BTreeMap<usize, Rc<Environment>>> = const { RefCell::new(BTreeMap::new()) };
}

pub(crate) struct Environment {
    pub authority: String,
    evaluations: Cell<usize>,
    source_bytes: Cell<usize>,
    failures: Cell<usize>,
    active: RefCell<Weak<Preparation>>,
    warned: Cell<bool>,
    source_generations: Cell<usize>,
    transformed_bytes: Cell<usize>,
    source_modules: Cell<usize>,
    source_warned: Cell<bool>,
    source_failures: Cell<usize>,
}
impl Environment {
    pub fn get(env: Env) -> napi::Result<Rc<Self>> {
        let key = env.raw() as usize;
        if let Some(environment) = ENVIRONMENTS.with(|all| all.borrow().get(&key).cloned()) {
            return Ok(environment);
        }
        let environment = Rc::new(Self {
            authority: tembra_native::random_incarnation()
                .map_err(|e| napi::Error::from_reason(e.to_string()))?,
            evaluations: Cell::new(0),
            source_bytes: Cell::new(0),
            failures: Cell::new(0),
            active: RefCell::default(),
            warned: Cell::new(false),
            source_generations: Cell::new(0),
            transformed_bytes: Cell::new(0),
            source_modules: Cell::new(0),
            source_warned: Cell::new(false),
            source_failures: Cell::new(0),
        });
        env.add_env_cleanup_hook(key, |key| {
            ENVIRONMENTS.with(|all| {
                all.borrow_mut().remove(&key);
            });
        })?;
        ENVIRONMENTS.with(|all| {
            all.borrow_mut().insert(key, environment.clone());
        });
        Ok(environment)
    }
    pub fn begin(self: &Rc<Self>) -> Result<Rc<Preparation>> {
        if self.active.borrow().upgrade().is_some() {
            return Err(RuntimeError::unavailable(
                "loader_busy",
                "another Node facet preparation is in flight",
            ));
        }
        let preparation = Rc::new(Preparation {
            environment: self.clone(),
            costs: RefCell::new(VecDeque::new()),
            reserved: Cell::new(false),
            source_costs: RefCell::new(VecDeque::new()),
            source_reserved: Cell::new(false),
            stage: Cell::new("validating"),
            started: Instant::now(),
        });
        *self.active.borrow_mut() = Rc::downgrade(&preparation);
        Ok(preparation)
    }
    pub fn diagnostics(&self) -> Value {
        let active = self.active.borrow().upgrade();
        let exhausted = self.evaluations.get() >= MAX_EVALUATIONS
            || self.source_bytes.get() >= MAX_SOURCE_BYTES;
        json!({
            "policy": "javascript-lifetime-budget", "physicalUnloadSupported": false,
            "evaluations": self.evaluations.get(), "sourceBytes": self.source_bytes.get(),
            "maxEvaluations": MAX_EVALUATIONS, "maxSourceBytes": MAX_SOURCE_BYTES,
            "maxArtifactBytes": MAX_ARTIFACT_BYTES,
            "remainingEvaluations": MAX_EVALUATIONS - self.evaluations.get(),
            "remainingSourceBytes": MAX_SOURCE_BYTES - self.source_bytes.get(),
            "evaluationFailures": self.failures.get(), "warning": self.warned.get(),
            "requiresProcessRotation": exhausted,
            "source": {
                "profile":"node-source-v1", "physicalUnloadSupported":false,
                "evaluations":self.source_generations.get(),"sourceBytes":self.transformed_bytes.get(),
                "modules":self.source_modules.get(),"maxEvaluations":MAX_SOURCE_GENERATIONS,
                "evaluationFailures":self.source_failures.get(),
                "maxSourceBytes":MAX_TRANSFORMED_BYTES,"maxModules":MAX_SOURCE_MODULES,
                "maxModuleBytes":MAX_SOURCE_MODULE_BYTES,
                "warning":self.source_generations.get()>=MAX_SOURCE_GENERATIONS*3/4 || self.transformed_bytes.get()>=MAX_TRANSFORMED_BYTES*3/4 || self.source_modules.get()>=MAX_SOURCE_MODULES*3/4,
                "requiresProcessRotation":self.source_generations.get()>=MAX_SOURCE_GENERATIONS || self.transformed_bytes.get()>=MAX_TRANSFORMED_BYTES || self.source_modules.get()>=MAX_SOURCE_MODULES
            },
            "inFlight": active.as_ref().map(|p| json!({
                "stage": p.stage.get(), "elapsedMs": p.started.elapsed().as_millis(),
                "reservedEvaluations": p.costs.borrow().len() + p.source_costs.borrow().len(),
                "reservedSourceBytes": p.costs.borrow().iter().sum::<usize>() + p.source_costs.borrow().iter().sum::<usize>()
            }))
        })
    }
}

/// One preparation owns admission until the facade physically finishes and explicitly frees it.
pub(crate) struct Preparation {
    environment: Rc<Environment>,
    costs: RefCell<VecDeque<usize>>,
    reserved: Cell<bool>,
    source_costs: RefCell<VecDeque<usize>>,
    source_reserved: Cell<bool>,
    pub stage: Cell<&'static str>,
    started: Instant,
}
impl Preparation {
    /// Reserves source entry points; transitive transforms are charged before transformation.
    pub fn reserve_sources(&self, costs: Vec<usize>, version: u64) -> Result<()> {
        if self.source_reserved.get() || costs.iter().any(|bytes| *bytes > MAX_SOURCE_MODULE_BYTES)
        {
            return Err(RuntimeError::invalid(
                "source_size",
                "source entry exceeds 2 MiB or reservation repeated",
            ));
        }
        let bytes = costs
            .iter()
            .try_fold(0usize, |sum, cost| sum.checked_add(*cost))
            .ok_or_else(|| RuntimeError::invalid("source_size", "source size overflow"))?;
        self.check_source(
            "evaluations",
            self.environment.source_generations.get(),
            costs.len(),
            MAX_SOURCE_GENERATIONS,
            version,
        )?;
        self.check_source(
            "sourceBytes",
            self.environment.transformed_bytes.get(),
            bytes,
            MAX_TRANSFORMED_BYTES,
            version,
        )?;
        self.check_source(
            "modules",
            self.environment.source_modules.get(),
            costs.len(),
            MAX_SOURCE_MODULES,
            version,
        )?;
        *self.source_costs.borrow_mut() = costs.into();
        self.source_reserved.set(true);
        Ok(())
    }
    fn check_source(
        &self,
        dimension: &str,
        used: usize,
        requested: usize,
        limit: usize,
        version: u64,
    ) -> Result<()> {
        if requested > limit.saturating_sub(used) {
            return Err(RuntimeError::unavailable("loader_budget_exhausted","Node source lifetime budget exhausted")
                .with_details(json!({"profile":"node-source-v1","dimension":dimension,"used":used,"requested":requested,"limit":limit,"codeVersion":version})));
        }
        Ok(())
    }
    pub fn charge_source_module(&self, bytes: usize, version: u64) -> Result<()> {
        if bytes > MAX_SOURCE_MODULE_BYTES {
            return Err(RuntimeError::invalid(
                "source_size",
                "source module exceeds 2 MiB",
            ));
        }
        // Keep all not-yet-submitted entry reservations available when a dependency is admitted.
        let reserved_bytes = self.source_costs.borrow().iter().sum::<usize>();
        let reserved_modules = self.source_costs.borrow().len();
        self.check_source(
            "sourceBytes",
            self.environment.transformed_bytes.get() + reserved_bytes,
            bytes,
            MAX_TRANSFORMED_BYTES,
            version,
        )?;
        self.check_source(
            "modules",
            self.environment.source_modules.get() + reserved_modules,
            1,
            MAX_SOURCE_MODULES,
            version,
        )?;
        self.environment
            .transformed_bytes
            .set(self.environment.transformed_bytes.get() + bytes);
        self.environment
            .source_modules
            .set(self.environment.source_modules.get() + 1);
        Ok(())
    }
    pub fn submit_source(&self, version: u64) -> Result<bool> {
        let bytes = self.source_costs.borrow_mut().pop_front().ok_or_else(|| {
            RuntimeError::invalid("loader_reservation", "source entry was not reserved")
        })?;
        self.charge_source_module(bytes, version)?;
        self.environment
            .source_generations
            .set(self.environment.source_generations.get() + 1);
        self.stage.set("evaluating");
        let threshold = self.environment.source_generations.get() >= MAX_SOURCE_GENERATIONS * 3 / 4
            || self.environment.transformed_bytes.get() >= MAX_TRANSFORMED_BYTES * 3 / 4
            || self.environment.source_modules.get() >= MAX_SOURCE_MODULES * 3 / 4;
        Ok(threshold && !self.environment.source_warned.replace(true))
    }
    /// Reserves an entire validated batch. Refusal changes no counters and submits no module.
    pub fn reserve(&self, costs: Vec<usize>, code_version: u64) -> Result<()> {
        if self.reserved.get() || costs.iter().any(|bytes| *bytes > MAX_ARTIFACT_BYTES) {
            return Err(RuntimeError::invalid(
                "loader_reservation",
                "invalid or repeated source reservation",
            ));
        }
        let bytes = costs
            .iter()
            .try_fold(0usize, |sum, cost| sum.checked_add(*cost))
            .ok_or_else(|| RuntimeError::invalid("loader_reservation", "source size overflow"))?;
        for (dimension, used, requested, limit) in [
            (
                "evaluations",
                self.environment.evaluations.get(),
                costs.len(),
                MAX_EVALUATIONS,
            ),
            (
                "sourceBytes",
                self.environment.source_bytes.get(),
                bytes,
                MAX_SOURCE_BYTES,
            ),
        ] {
            if requested > limit - used {
                return Err(RuntimeError::unavailable("loader_budget_exhausted", "Node ESM lifetime budget exhausted")
                    .with_details(json!({"dimension":dimension,"used":used,"requested":requested,"limit":limit,"codeVersion":code_version})));
            }
        }
        *self.costs.borrow_mut() = costs.into();
        self.reserved.set(true);
        self.stage.set("reserved");
        Ok(())
    }
    /// Charges one submitted identity permanently, including failed evaluations.
    pub fn submit(&self) -> Result<bool> {
        let cost = self.costs.borrow_mut().pop_front().ok_or_else(|| {
            RuntimeError::invalid("loader_reservation", "module was not reserved")
        })?;
        self.environment
            .evaluations
            .set(self.environment.evaluations.get() + 1);
        self.environment
            .source_bytes
            .set(self.environment.source_bytes.get() + cost);
        self.stage.set("evaluating");
        let threshold = self.environment.evaluations.get() >= MAX_EVALUATIONS * 3 / 4
            || self.environment.source_bytes.get() >= MAX_SOURCE_BYTES * 3 / 4;
        Ok(threshold && !self.environment.warned.replace(true))
    }
    pub fn failed(&self, source: bool) {
        let counter = if source {
            &self.environment.source_failures
        } else {
            &self.environment.failures
        };
        counter.set(counter.get().saturating_add(1));
    }
}

pub(crate) struct Generation {
    pub id: String,
    pub kind: &'static str,
    pub source_hash: RefCell<Option<String>>,
    created: Instant,
    session: Weak<Session>,
    pub callbacks: Cell<usize>,
    pub values: Cell<usize>,
    pub physical: Cell<usize>,
    activated: Cell<bool>,
    runs: RefCell<BTreeMap<usize, FacetSignal>>,
    run_serial: Cell<usize>,
}
impl Generation {
    pub fn new(session: &Rc<Session>, kind: &'static str) -> Result<Rc<Self>> {
        let generation = Rc::new(Self {
            id: tembra_native::random_incarnation()?,
            kind,
            source_hash: RefCell::new(None),
            created: Instant::now(),
            session: Rc::downgrade(session),
            callbacks: Cell::new(0),
            values: Cell::new(0),
            physical: Cell::new(0),
            activated: Cell::new(false),
            runs: RefCell::default(),
            run_serial: Cell::new(0),
        });
        session
            .generations
            .borrow_mut()
            .insert(generation.id.clone(), Rc::downgrade(&generation));
        Ok(generation)
    }
    pub fn retain_run(self: &Rc<Self>, scope: &FacetScope, recorder: &FacetRecorder) -> Result<()> {
        let id = self.run_serial.get().checked_add(1).ok_or_else(|| {
            RuntimeError::unavailable("generation_limit", "run identity exhausted")
        })?;
        self.run_serial.set(id);
        self.runs.borrow_mut().insert(id, scope.signal());
        recorder.retain_run_owner(Rc::new(RunLease {
            generation: self.clone(),
            id,
        }))?;
        self.activated.set(true);
        Ok(())
    }
    pub fn diagnostics(&self) -> Value {
        let runs = self.runs.borrow();
        let active = runs
            .values()
            .filter(|signal| !signal.is_cancelled())
            .count();
        json!({"id":self.id,"kind":self.kind,"sourceHash":*self.source_hash.borrow(),"ageMs":self.created.elapsed().as_millis(),
            "state": if !self.activated.get() { "prepared" } else if active > 0 { "managed" } else { "retiring" },
            "runOwners":runs.len(),"activeRuns":active,"callbacks":self.callbacks.get(),
            "values":self.values.get(),"physicalCallbacks":self.physical.get()})
    }
}
struct RunLease {
    generation: Rc<Generation>,
    id: usize,
}
impl Drop for RunLease {
    fn drop(&mut self) {
        self.generation.runs.borrow_mut().remove(&self.id);
    }
}
impl Drop for Generation {
    fn drop(&mut self) {
        if let Some(session) = self.session.upgrade() {
            session.generations.borrow_mut().remove(&self.id);
            let mut history = session.released_generations.borrow_mut();
            if history.len() == 32 {
                history.pop_front();
            }
            history.push_back(json!({"id":self.id,"kind":self.kind,"sourceHash":*self.source_hash.borrow(),"state":"managedReleased"}));
        }
    }
}

pub(crate) struct GenerationGuard<'a> {
    slot: &'a RefCell<Option<Rc<Generation>>>,
    prior: Option<Rc<Generation>>,
}
impl<'a> GenerationGuard<'a> {
    pub fn new(
        slot: &'a RefCell<Option<Rc<Generation>>>,
        generation: Option<Rc<Generation>>,
    ) -> Self {
        Self {
            slot,
            prior: slot.replace(generation),
        }
    }
}
impl Drop for GenerationGuard<'_> {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.prior.take();
    }
}

pub(crate) struct PhysicalLease(Option<Rc<Generation>>);
impl PhysicalLease {
    pub fn new(generation: Option<Rc<Generation>>) -> Self {
        if let Some(generation) = &generation {
            generation.physical.set(generation.physical.get() + 1);
        }
        Self(generation)
    }
}
impl Drop for PhysicalLease {
    fn drop(&mut self) {
        if let Some(generation) = &self.0 {
            generation.physical.set(generation.physical.get() - 1);
        }
    }
}
