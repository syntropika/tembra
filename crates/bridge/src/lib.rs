//! Metadata-only adapter protocol. User arguments, state values and callbacks never enter JSON here.
pub mod protocol;
use protocol::{ActionType, EffectType, KernelOp, StepType};

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::*;

pub use tembra_core as core;

#[derive(Clone, Default)]
pub struct Kernel {
    pub host: Host,
}

impl Kernel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Converts a metadata request to a result envelope. Malformed input executes no command.
    pub fn request(&self, request: &str) -> String {
        let result = if request.len() > 1024 * 1024 {
            Err(Error::invalid(
                Reason::MetadataSize,
                "metadata exceeds one mebibyte",
            ))
        } else {
            serde_json::from_str::<Value>(request)
                .map_err(|e| Error::invalid(Reason::Metadata, e.to_string()))
                .and_then(|value| self.execute(&value))
        };
        match result { Ok(value) => json!({"ok":value}), Err(error) => json!({"error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}}) }.to_string()
    }

    pub fn execute(&self, request: &Value) -> Result<Value> {
        let name = text(request, "op")?;
        let op = KernelOp::parse(name).ok_or_else(|| {
            Error::new(
                Code::Unsupported,
                Reason::Operation,
                format!("operation {name} is unavailable in this build"),
            )
        })?;
        let h = &self.host;
        let id = |key| identity(request, key);
        let unit = || Ok(Value::Null);
        match op {
            KernelOp::Allocate => Ok(json!(h.allocate_handle()?)),
            KernelOp::ScopeRegister => {
                h.register_scope(Scope {
                    id: id("scope")?,
                    owner: owner(request)?,
                    facet: id("facet")?,
                    parents: numbers_optional(request, "parents")?,
                    dependencies: numbers_optional(request, "dependencies")?,
                    resources: numbers_optional(request, "resources")?,
                    state_version: optional_number(request, "stateVersion")?,
                    migrate: request["migrate"].as_bool().unwrap_or(false),
                    active: false,
                    closing: false,
                })?;
                unit()
            }
            KernelOp::ScopeExists => Ok(json!(h.scope_exists(id("scope")?))),
            KernelOp::ScopesForOwner => Ok(json!(h.scopes_for_owner(owner(request)?))),
            KernelOp::ScopesForFacet => Ok(json!(h.scopes_for_facet(id("facet")?))),
            KernelOp::ScopeAddResource => {
                h.add_scope_resource(id("scope")?, id("resource")?)?;
                unit()
            }
            KernelOp::ScopeActivate => Ok(json!(h.activate_scope(id("scope")?)?)),
            KernelOp::ScopesCleanup => Ok(json!(h.cleanup_scopes(
                numbers(request, "scopes")?,
                optional_identity(request, "object")?
            )?)),
            KernelOp::ScopesReloadRegistered => Ok(json!(
                h.reload_registered_scopes(id("ticket")?, wide(request, "deadline")?)?
            )),
            KernelOp::ScopesReload => {
                let pairs = array(request, "replacements")?
                    .iter()
                    .map(|pair| {
                        Ok(Replacement {
                            previous: optional_identity(pair, "previous")?,
                            next: optional_identity(pair, "next")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!(h.reload_scopes(
                    id("ticket")?,
                    pairs,
                    wide(request, "deadline")?
                )?))
            }
            KernelOp::ProgramRun => {
                h.run_program(id("program")?)?;
                unit()
            }
            KernelOp::ProgramNext => Ok(step_value(h.next_step(id("program")?)?)),
            KernelOp::ProgramCompleteStep => {
                let results = array(request, "results")?
                    .iter()
                    .map(|result| {
                        let error =
                            result
                                .get("error")
                                .filter(|value| !value.is_null())
                                .map(|error| {
                                    Error::new(
                                        parse_code(error["code"].as_str().unwrap_or("invalid")),
                                        error["reason"].as_str().unwrap_or("execution"),
                                        error["message"].as_str().unwrap_or("execution failed"),
                                    )
                                });
                        Ok(Completion {
                            value: optional_identity(result, "value")?,
                            error,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                h.complete_step(id("program")?, results)?;
                unit()
            }
            KernelOp::ProgramCommit => {
                h.commit_program(id("program")?)?;
                unit()
            }
            KernelOp::ProgramAbort => {
                h.abort_program(id("program")?)?;
                unit()
            }
            KernelOp::ProgramForget => {
                h.forget_program(id("program")?)?;
                unit()
            }
            KernelOp::Features => Ok(
                json!({"hooks":cfg!(feature="hooks"),"subscriptions":cfg!(feature="subscriptions"),"views":cfg!(feature="views"),"telemetry":cfg!(feature="telemetry")}),
            ),
            KernelOp::Prepare => {
                let facets = array(request, "facets")?
                    .iter()
                    .map(parse_facet)
                    .collect::<Result<Vec<_>>>()?;
                let remove = strings_optional(request, "remove")?;
                let reload = h.prepare(facets, remove)?;
                Ok(
                    json!({"id":reload.id,"codeVersion":reload.code_version,"facets":reload.facets.iter().map(facet_value).collect::<Vec<_>>(),"objects":reload.objects,"providers":reload.providers}),
                )
            }
            KernelOp::Gate => Ok(json!(h.gate(id("ticket")?, &numbers(request, "objects")?)?)),
            KernelOp::GateFacets => Ok(json!(
                h.gate_facets(id("ticket")?, &numbers(request, "owners")?)?
            )),
            KernelOp::Commit => {
                let migrations = request
                    .get("migrations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        Ok(Migration {
                            object: identity(m, "object")?,
                            expected_revision: wide(m, "revision")?,
                            value: identity(m, "value")?,
                            state_version: number(m, "version")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!(h.commit(id("ticket")?, migrations)?))
            }
            KernelOp::Abort => {
                h.abort(id("ticket")?)?;
                unit()
            }
            KernelOp::Facets => Ok(json!(
                h.installed_facets()
                    .iter()
                    .map(facet_value)
                    .collect::<Vec<_>>()
            )),
            KernelOp::Facet => Ok(facet_value(&h.facet(id("id")?)?)),
            KernelOp::FacetNamed => {
                let name = text(request, "name")?;
                let facet = match optional_identity(request, "ticket")? {
                    Some(ticket) => h.prepared_facet(ticket, name)?,
                    None => h.facet_named(name)?,
                };
                Ok(facet_value(&facet))
            }
            KernelOp::NextDeadline => Ok(json!(h.next_deadline())),
            KernelOp::Address => {
                let a = h.object_address(id("object")?)?;
                Ok(json!({"service":a.service,"key":a.key,"principal":a.principal}))
            }
            KernelOp::Acquire => {
                let address = Address {
                    service: text(request, "service")?.into(),
                    key: strings(request, "key")?,
                    principal: text(request, "principal")?.into(),
                };
                let result = h.acquire(
                    address,
                    optional_number(request, "apiVersion")?.unwrap_or(0),
                    strings_optional(request, "methods")?.into_iter().collect(),
                )?;
                Ok(
                    json!({"reference":result.reference,"object":result.object,"facet":result.facet,"codeVersion":result.code_version,"activate":result.activate}),
                )
            }
            KernelOp::Ready => {
                h.ready(id("object")?)?;
                unit()
            }
            KernelOp::Dependency => {
                h.dependency(id("object")?, id("reference")?)?;
                unit()
            }
            KernelOp::Undepend => {
                h.detach_dependency(id("object")?, id("reference")?)?;
                unit()
            }
            KernelOp::Fail => {
                h.fail(id("object")?)?;
                unit()
            }
            KernelOp::Status => Ok(json!(h.status(id("reference")?)?.as_str())),
            KernelOp::Release => {
                h.release(id("reference")?)?;
                unit()
            }
            KernelOp::Forget => {
                h.forget_reference(id("reference")?)?;
                unit()
            }
            KernelOp::Root => Ok(json!(h.begin_root(text(request, "principal")?)?)),
            KernelOp::CleanupRoot => Ok(json!(h.cleanup_root(text(request, "principal")?)?)),
            KernelOp::RootPrincipal => Ok(json!(h.root_principal(id("root")?)?)),
            KernelOp::CheckContext => {
                let root = id("root")?;
                h.check_root(root)?;
                if let Some(call) = optional_identity(request, "call")?
                    && h.invocation(call)?.root != root
                {
                    return Err(Error::invalid(
                        Reason::Context,
                        "invocation belongs to another root",
                    ));
                }
                unit()
            }
            KernelOp::CheckRoot => {
                h.check_root(id("root")?)?;
                unit()
            }
            KernelOp::EndRoot => {
                h.end_root(id("root")?)?;
                unit()
            }
            KernelOp::CancelRoot => {
                h.cancel_root(id("root")?)?;
                unit()
            }
            KernelOp::BeginCall => Ok(invocation_value(&h.begin_call(
                id("reference")?,
                id("root")?,
                text(request, "method")?,
            )?)),
            KernelOp::BeginOperation => Ok(invocation_value(&h.begin_operation(
                id("facet")?,
                id("root")?,
                id("operation")?,
                optional_identity(request, "object")?,
            )?)),
            KernelOp::FinishCall => {
                h.finish_call(id("call")?)?;
                unit()
            }
            KernelOp::Invocation => Ok(invocation_value(&h.invocation(id("call")?)?)),
            KernelOp::StateInit => Ok(snapshot_value(h.initialize_state(
                id("object")?,
                id("value")?,
                number(request, "version")?,
            )?)),
            KernelOp::Snapshot => Ok(snapshot_value(h.snapshot(id("object")?)?)),
            KernelOp::StateAdopt => Ok(snapshot_value(h.adopt_state(
                id("call")?,
                id("object")?,
                wide(request, "revision")?,
                id("value")?,
            )?)),
            KernelOp::Task => Ok(json!(h.begin_task(owner(request)?)?)),
            KernelOp::TaskRoot => Ok(json!(
                h.task_root(id("task")?, text(request, "principal")?)?
            )),
            KernelOp::StopTask => {
                h.stop_task(id("task")?)?;
                unit()
            }
            KernelOp::FinishTask => {
                h.finish_task(id("task")?)?;
                unit()
            }
            KernelOp::Resource => {
                let r = h.resource(
                    owner(request)?,
                    text(request, "name")?.into(),
                    number(request, "version")?,
                    request["exclusive"].as_bool().unwrap_or(false),
                    numbers_optional(request, "dependencies")?,
                )?;
                Ok(json!({"id":r.id,"value":r.value}))
            }
            KernelOp::ResourceOpened => {
                h.resource_opened(id("resource")?, id("value")?)?;
                unit()
            }
            KernelOp::ResourceValue => Ok(json!(h.resource_value(id("resource")?)?)),
            KernelOp::CloseResource => Ok(json!(
                h.close_resource_before(id("resource")?, optional_number(request, "before")?)?
            )),
            KernelOp::ResourceClosed => {
                h.resource_closed(id("resource")?)?;
                unit()
            }
            KernelOp::Disposed => {
                h.disposed(id("object")?)?;
                unit()
            }
            KernelOp::Tick => {
                h.tick(wide(request, "now")?)?;
                unit()
            }
            KernelOp::NextWake => Ok(json!(h.next_wake())),
            KernelOp::NextIdle => Ok(json!(h.next_idle())),
            KernelOp::Close => {
                h.close();
                unit()
            }
            KernelOp::FinishClose => {
                h.finish_close()?;
                unit()
            }
            KernelOp::Diagnostics => {
                let d = h.diagnostics();
                Ok(
                    json!({"objects":d.objects,"references":d.references,"roots":d.roots,"calls":d.calls,"resources":d.resources,"tasks":d.tasks,"facets":d.facets,"closing":d.closing,"codeVersion":h.version()}),
                )
            }
            KernelOp::Drain => Ok(json!(
                h.drain().into_iter().map(action_value).collect::<Vec<_>>()
            )),
            #[cfg(feature = "views")]
            KernelOp::Restrict => {
                h.restrict(
                    id("reference")?,
                    strings(request, "methods")?.into_iter().collect(),
                )?;
                unit()
            }
            #[cfg(feature = "hooks")]
            KernelOp::Contribute => {
                h.contribute(Contribution {
                    facet: id("facet")?,
                    target: text(request, "target")?.into(),
                    hook: text(request, "hook")?.into(),
                    operation: id("operation")?,
                    priority: request["priority"]
                        .as_i64()
                        .unwrap_or(0)
                        .try_into()
                        .map_err(|_| Error::invalid(Reason::Priority, "priority exceeds i32"))?,
                    ordinal: number(request, "ordinal")?,
                })?;
                unit()
            }
            #[cfg(feature = "hooks")]
            KernelOp::HookPlan => Ok(json!(&*h.hook_plan(
                id("root")?,
                text(request, "target")?,
                text(request, "hook")?
            )?)),
            #[cfg(feature = "subscriptions")]
            KernelOp::Listen => Ok(json!(h.listen(
                id("scope")?,
                optional_identity(request, "previous")?,
                text(request, "name")?.into(),
                id("source")?,
                optional_identity(request, "sizing")?.unwrap_or(0),
                id("operation")?,
                number(request, "maxPending")? as usize,
                optional_number(request, "maxBytes")?.map(|value| value as usize),
                request["coalesce"].as_bool().unwrap_or(false)
            )?)),
            #[cfg(feature = "subscriptions")]
            KernelOp::Subscribe => Ok(json!(h.subscribe(
                owner(request)?,
                id("source")?,
                id("operation")?,
                number(request, "maxPending")? as usize,
                optional_number(request, "maxBytes")?.map(|v| v as usize),
                request["coalesce"].as_bool().unwrap_or(false)
            )?)),
            #[cfg(feature = "subscriptions")]
            KernelOp::Enqueue => {
                h.enqueue(
                    id("subscription")?,
                    Delivery {
                        payload: id("payload")?,
                        bytes: optional_number(request, "bytes")?.unwrap_or(0) as usize,
                    },
                )?;
                unit()
            }
            #[cfg(feature = "subscriptions")]
            KernelOp::NextDelivery => Ok(h
                .next_delivery(id("subscription")?)?
                .map_or(Value::Null, |d| json!(d.payload))),
            #[cfg(feature = "subscriptions")]
            KernelOp::FinishDelivery => {
                h.finish_delivery(id("subscription")?)?;
                unit()
            }
            #[cfg(feature = "subscriptions")]
            KernelOp::Unsubscribe => {
                h.unsubscribe(id("subscription")?)?;
                unit()
            }
            #[allow(unreachable_patterns)] // Optional feature arms are absent in minimal builds.
            _ => Err(Error::new(
                Code::Unsupported,
                Reason::Operation,
                format!("operation {} is unavailable in this build", op.as_str()),
            )),
        }
    }
}

fn step_value(step: Step) -> Value {
    match step {
        Step::Wait { deadline } => {
            json!({"type":StepType::Wait.as_str(), "deadline":deadline.map(|deadline|deadline.to_string())})
        }
        Step::Prepared => json!({"type":StepType::Prepared.as_str()}),
        Step::Complete => json!({"type":StepType::Complete.as_str()}),
        Step::Failed(error) => {
            json!({"type":StepType::Failed.as_str(),"error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}})
        }
        Step::Execute(effects) => {
            json!({"type":StepType::Execute.as_str(),"effects":effects.into_iter().map(effect_value).collect::<Vec<_>>()})
        }
    }
}
fn error_value(error: Error) -> Value {
    json!({"code":error.code.as_str(),"reason":error.reason,"message":error.message})
}
fn effect_value(effect: Effect) -> Value {
    match effect {
        Effect::Stop {
            scope,
            keep_listeners,
        } => json!({"type":EffectType::Stop.as_str(),"scope":scope,"keepListeners":keep_listeners}),
        Effect::Open { resource } => json!({"type":EffectType::Open.as_str(),"resource":resource}),
        Effect::Close { resource } => {
            json!({"type":EffectType::Close.as_str(),"resource":resource})
        }
        Effect::Restore { previous, resource } => {
            json!({"type":EffectType::Restore.as_str(),"previous":previous,"resource":resource})
        }
        Effect::Prepare { scope, previous } => {
            json!({"type":EffectType::Prepare.as_str(),"scope":scope,"previous":previous})
        }
        Effect::Validate { scope, value } => {
            json!({"type":EffectType::Validate.as_str(),"scope":scope,"value":value})
        }
        Effect::Initialize { scope } => {
            json!({"type":EffectType::Initialize.as_str(),"scope":scope})
        }
        Effect::Migrate {
            scope,
            value,
            version,
        } => {
            json!({"type":EffectType::Migrate.as_str(),"scope":scope,"value":value,"version":version})
        }
        Effect::Adopt { replacements } => {
            json!({"type":EffectType::Adopt.as_str(),"replacements":replacements.into_iter().map(|pair|json!({"previous":pair.previous,"next":pair.next})).collect::<Vec<_>>()})
        }
        Effect::Start { scope } => json!({"type":EffectType::Start.as_str(),"scope":scope}),
        Effect::Drop { scope } => json!({"type":EffectType::Drop.as_str(),"scope":scope}),
    }
}

fn parse_code(code: &str) -> Code {
    Code::parse(code).unwrap_or(Code::Invalid)
}

fn action_value(action: Action) -> Value {
    match action {
        Action::Execute { program, effects } => {
            json!({"type":ActionType::Execute.as_str(),"id":program,"effects":effects.into_iter().map(effect_value).collect::<Vec<_>>()})
        }
        Action::CancelEffects { program, effects } => {
            json!({"type":ActionType::CancelEffects.as_str(),"id":program,"effects":effects.into_iter().map(effect_value).collect::<Vec<_>>()})
        }
        Action::ProgramPrepared(id) => json!({"type":ActionType::ProgramPrepared.as_str(),"id":id}),
        Action::ProgramComplete { program, error } => {
            json!({"type":ActionType::ProgramComplete.as_str(),"id":program,"error":error.map(error_value)})
        }
        Action::DisposeObject(id) => json!({"type":ActionType::Dispose.as_str(),"id":id}),
        Action::CloseResource(id) => json!({"type":ActionType::CloseResource.as_str(),"id":id}),
        Action::DropValue(id) => json!({"type":ActionType::DropValue.as_str(),"id":id}),
        Action::CancelRoot(id) => json!({"type":ActionType::CancelRoot.as_str(),"id":id}),
        Action::CancelTask(id) => json!({"type":ActionType::CancelTask.as_str(),"id":id}),
        Action::RetireFacet(id) => json!({"type":ActionType::RetireFacet.as_str(),"id":id}),
        Action::StateChanged { object, revision } => {
            json!({"type":ActionType::State.as_str(),"id":object,"revision":revision.to_string()})
        }
        #[cfg(feature = "subscriptions")]
        Action::SubscriptionFailed(id) => {
            json!({"type":ActionType::SubscriptionFailed.as_str(),"id":id})
        }
        #[cfg(feature = "subscriptions")]
        Action::DetachSubscription(id) => {
            json!({"type":ActionType::DetachSubscription.as_str(),"id":id})
        }
        #[cfg(feature = "subscriptions")]
        Action::Deliver {
            subscription,
            payload,
            operation,
        } => {
            json!({"type":ActionType::Deliver.as_str(),"id":subscription,"payload":payload,"operation":operation})
        }
    }
}
fn invocation_value(i: &Invocation) -> Value {
    json!({"id":i.id,"root":i.root,"object":i.object,"facet":i.facet,"codeVersion":i.code_version,"method":i.method})
}
fn snapshot_value(s: Snapshot) -> Value {
    json!({"value":s.value,"revision":s.revision.to_string(),"version":s.version})
}
fn facet_value(f: &InstalledFacet) -> Value {
    json!({"id":f.id,"owner":f.owner,"codeVersion":f.code_version,"name":f.declaration.name,"apiVersion":f.declaration.api_version,
        "services":f.declaration.services.iter().map(|s|json!({"name":s.name,"methods":s.methods,"lifetime":s.lifetime.as_str(),"linger":s.linger_ms})).collect::<Vec<_>>(),"hooks":f.declaration.hooks,
        "requires":f.declaration.requires.iter().map(|requirement|json!({"facet":requirement.facet,"apiVersion":requirement.api_version,"services":requirement.methods,"hooks":requirement.hooks})).collect::<Vec<_>>()})
}
fn parse_facet(f: &Value) -> Result<Facet> {
    let services = array(f, "services")?
        .iter()
        .map(|s| {
            Ok(Service {
                name: text(s, "name")?.into(),
                methods: strings(s, "methods")?.into_iter().collect(),
                lifetime: match s["lifetime"].as_str().unwrap_or("referenced") {
                    "host" => Lifetime::Host,
                    "referenced" => Lifetime::Referenced,
                    _ => return Err(Error::invalid(Reason::Lifetime, "unknown lifetime")),
                },
                linger_ms: optional_number(s, "linger")?.unwrap_or(0),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let requires = f
        .get("requires")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| {
            let mut methods = BTreeMap::new();
            if let Some(map) = r["services"].as_object() {
                for (key, value) in map {
                    methods.insert(
                        key.clone(),
                        value
                            .as_array()
                            .ok_or_else(|| {
                                Error::invalid(Reason::Requires, "methods must be an array")
                            })?
                            .iter()
                            .map(|v| {
                                v.as_str().map(String::from).ok_or_else(|| {
                                    Error::invalid(Reason::Requires, "method must be text")
                                })
                            })
                            .collect::<Result<BTreeSet<_>>>()?,
                    );
                }
            }
            Ok(Requirement {
                facet: text(r, "facet")?.into(),
                api_version: number(r, "apiVersion")?,
                methods,
                hooks: strings_optional(r, "hooks")?.into_iter().collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Facet {
        name: text(f, "name")?.into(),
        api_version: number(f, "apiVersion")?,
        services,
        requires,
        hooks: strings_optional(f, "hooks")?.into_iter().collect(),
    })
}
fn owner(v: &Value) -> Result<Owner> {
    match v["owner"]["kind"].as_str() {
        Some("host") => Ok(Owner::Host),
        Some("facet") => Ok(Owner::Facet(identity(&v["owner"], "id")?)),
        Some("object") => Ok(Owner::Object(identity(&v["owner"], "id")?)),
        _ => Err(Error::invalid(Reason::Owner, "invalid owner")),
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| Error::invalid(Reason::Metadata, format!("{key} must be text")))
}
fn number(v: &Value, key: &str) -> Result<u32> {
    v[key]
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| Error::invalid(Reason::Metadata, format!("{key} must be a u32")))
}
fn optional_number(v: &Value, key: &str) -> Result<Option<u32>> {
    if v.get(key).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        number(v, key).map(Some)
    }
}
fn wide(v: &Value, key: &str) -> Result<u64> {
    v[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v[key].as_u64())
        .ok_or_else(|| {
            Error::invalid(
                Reason::Metadata,
                format!("{key} must be an unsigned revision or clock"),
            )
        })
}
fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v[key]
        .as_array()
        .ok_or_else(|| Error::invalid(Reason::Metadata, format!("{key} must be an array")))
}
fn strings(v: &Value, key: &str) -> Result<Vec<String>> {
    array(v, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| Error::invalid(Reason::Metadata, "expected text"))
        })
        .collect()
}
fn strings_optional(v: &Value, key: &str) -> Result<Vec<String>> {
    if v.get(key).is_none() {
        Ok(Vec::new())
    } else {
        strings(v, key)
    }
}
fn identity_value(v: &Value) -> Result<Id> {
    v.as_u64()
        .filter(|id| *id <= MAX_ID)
        .ok_or_else(|| Error::invalid(Reason::Metadata, "identity must be a safe unsigned integer"))
}
fn identity(v: &Value, key: &str) -> Result<Id> {
    identity_value(&v[key])
}
fn optional_identity(v: &Value, key: &str) -> Result<Option<Id>> {
    if v.get(key).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        identity(v, key).map(Some)
    }
}
fn numbers(v: &Value, key: &str) -> Result<Vec<Id>> {
    array(v, key)?.iter().map(identity_value).collect()
}
fn numbers_optional(v: &Value, key: &str) -> Result<Vec<Id>> {
    if v.get(key).is_none() {
        Ok(Vec::new())
    } else {
        numbers(v, key)
    }
}
