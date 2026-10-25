//! Metadata-only adapter protocol. User arguments, state values and callbacks never enter JSON here.
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
                "metadata_size",
                "metadata exceeds one mebibyte",
            ))
        } else {
            serde_json::from_str::<Value>(request)
                .map_err(|e| Error::invalid("metadata", e.to_string()))
                .and_then(|value| self.execute(&value))
        };
        match result { Ok(value) => json!({"ok":value}), Err(error) => json!({"error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}}) }.to_string()
    }

    pub fn execute(&self, request: &Value) -> Result<Value> {
        let op = text(request, "op")?;
        let h = &self.host;
        let id = |key| number(request, key);
        let unit = || Ok(Value::Null);
        match op {
            "allocate" => Ok(json!(h.allocate_handle()?)),
            "scope-register" => {
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
            "scope-exists" => Ok(json!(h.scope_exists(id("scope")?))),
            "scopes-for-owner" => Ok(json!(h.scopes_for_owner(owner(request)?))),
            "scopes-for-facet" => Ok(json!(h.scopes_for_facet(id("facet")?))),
            "scope-add-resource" => {
                h.add_scope_resource(id("scope")?, id("resource")?)?;
                unit()
            }
            "scope-activate" => Ok(json!(h.activate_scope(id("scope")?)?)),
            "scopes-cleanup" => Ok(json!(h.cleanup_scopes(
                numbers(request, "scopes")?,
                optional_number(request, "object")?
            )?)),
            "scopes-reload-registered" => Ok(json!(
                h.reload_registered_scopes(id("ticket")?, wide(request, "deadline")?)?
            )),
            "scopes-reload" => {
                let pairs = array(request, "replacements")?
                    .iter()
                    .map(|pair| {
                        Ok(Replacement {
                            previous: optional_number(pair, "previous")?,
                            next: optional_number(pair, "next")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!(h.reload_scopes(
                    id("ticket")?,
                    pairs,
                    wide(request, "deadline")?
                )?))
            }
            "program-run" => {
                h.run_program(id("program")?)?;
                unit()
            }
            "program-next" => Ok(step_value(h.next_step(id("program")?)?)),
            "program-complete-step" => {
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
                            value: optional_number(result, "value")?,
                            error,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                h.complete_step(id("program")?, results)?;
                unit()
            }
            "program-commit" => {
                h.commit_program(id("program")?)?;
                unit()
            }
            "program-abort" => {
                h.abort_program(id("program")?)?;
                unit()
            }
            "program-forget" => {
                h.forget_program(id("program")?)?;
                unit()
            }
            "features" => Ok(
                json!({"hooks":cfg!(feature="hooks"),"subscriptions":cfg!(feature="subscriptions"),"views":cfg!(feature="views"),"telemetry":cfg!(feature="telemetry")}),
            ),
            "prepare" => {
                let facets = array(request, "facets")?
                    .iter()
                    .map(parse_facet)
                    .collect::<Result<Vec<_>>>()?;
                let remove = strings_optional(request, "remove")?;
                let reload = h.prepare(facets, remove)?;
                Ok(
                    json!({"id":reload.id,"codeVersion":reload.code_version,"facets":reload.facets.iter().map(facet_value).collect::<Vec<_>>(),"objects":reload.objects}),
                )
            }
            "gate" => Ok(json!(h.gate(id("ticket")?, &numbers(request, "objects")?)?)),
            "gate-facets" => Ok(json!(
                h.gate_facets(id("ticket")?, &numbers(request, "owners")?)?
            )),
            "commit" => {
                let migrations = request
                    .get("migrations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        Ok(Migration {
                            object: number(m, "object")?,
                            expected_revision: wide(m, "revision")?,
                            value: number(m, "value")?,
                            state_version: number(m, "version")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!(h.commit(id("ticket")?, migrations)?))
            }
            "abort" => {
                h.abort(id("ticket")?)?;
                unit()
            }
            "facets" => Ok(json!(
                h.installed_facets()
                    .iter()
                    .map(facet_value)
                    .collect::<Vec<_>>()
            )),
            "facet" => Ok(facet_value(&h.facet(id("id")?)?)),
            "facet-named" => Ok(facet_value(&h.facet_named(text(request, "name")?)?)),
            "next-deadline" => Ok(json!(h.next_deadline())),
            "address" => {
                let a = h.object_address(id("object")?)?;
                Ok(json!({"service":a.service,"key":a.key,"principal":a.principal}))
            }
            "acquire" => {
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
            "ready" => {
                h.ready(id("object")?)?;
                unit()
            }
            "dependency" => {
                h.dependency(id("object")?, id("reference")?)?;
                unit()
            }
            "undepend" => {
                h.detach_dependency(id("object")?, id("reference")?)?;
                unit()
            }
            "fail" => {
                h.fail(id("object")?)?;
                unit()
            }
            "status" => Ok(json!(match h.status(id("reference")?)? {
                Status::Loading => "loading",
                Status::Live => "live",
                Status::Released => "released",
                Status::Gone => "gone",
            })),
            "release" => {
                h.release(id("reference")?)?;
                unit()
            }
            "forget" => {
                h.forget_reference(id("reference")?)?;
                unit()
            }
            "root" => Ok(json!(h.begin_root(text(request, "principal")?)?)),
            "cleanup-root" => Ok(json!(h.cleanup_root(text(request, "principal")?)?)),
            "root-principal" => Ok(json!(h.root_principal(id("root")?)?)),
            "check-context" => {
                let root = id("root")?;
                h.check_root(root)?;
                if let Some(call) = optional_number(request, "call")?
                    && h.invocation(call)?.root != root
                {
                    return Err(Error::invalid(
                        "context",
                        "invocation belongs to another root",
                    ));
                }
                unit()
            }
            "check-root" => {
                h.check_root(id("root")?)?;
                unit()
            }
            "end-root" => {
                h.end_root(id("root")?)?;
                unit()
            }
            "cancel-root" => {
                h.cancel_root(id("root")?)?;
                unit()
            }
            "begin-call" => Ok(invocation_value(&h.begin_call(
                id("reference")?,
                id("root")?,
                text(request, "method")?,
            )?)),
            "begin-operation" => Ok(invocation_value(&h.begin_operation(
                id("facet")?,
                id("root")?,
                id("operation")?,
                optional_number(request, "object")?,
            )?)),
            "finish-call" => {
                h.finish_call(id("call")?)?;
                unit()
            }
            "invocation" => Ok(invocation_value(&h.invocation(id("call")?)?)),
            "state-init" => Ok(snapshot_value(h.initialize_state(
                id("object")?,
                id("value")?,
                id("version")?,
            )?)),
            "snapshot" => Ok(snapshot_value(h.snapshot(id("object")?)?)),
            "state-adopt" => Ok(snapshot_value(h.adopt_state(
                id("call")?,
                id("object")?,
                wide(request, "revision")?,
                id("value")?,
            )?)),
            "task" => Ok(json!(h.begin_task(owner(request)?)?)),
            "task-root" => Ok(json!(
                h.task_root(id("task")?, text(request, "principal")?)?
            )),
            "stop-task" => {
                h.stop_task(id("task")?)?;
                unit()
            }
            "finish-task" => {
                h.finish_task(id("task")?)?;
                unit()
            }
            "resource" => {
                let r = h.resource(
                    owner(request)?,
                    text(request, "name")?.into(),
                    id("version")?,
                    request["exclusive"].as_bool().unwrap_or(false),
                    numbers_optional(request, "dependencies")?,
                )?;
                Ok(json!({"id":r.id,"value":r.value}))
            }
            "resource-opened" => {
                h.resource_opened(id("resource")?, id("value")?)?;
                unit()
            }
            "resource-value" => Ok(json!(h.resource_value(id("resource")?)?)),
            "close-resource" => Ok(json!(
                h.close_resource_before(id("resource")?, optional_number(request, "before")?)?
            )),
            "resource-closed" => {
                h.resource_closed(id("resource")?)?;
                unit()
            }
            "disposed" => {
                h.disposed(id("object")?)?;
                unit()
            }
            "tick" => {
                h.tick(wide(request, "now")?)?;
                unit()
            }
            "next-wake" => Ok(json!(h.next_wake())),
            "next-idle" => Ok(json!(h.next_idle())),
            "close" => {
                h.close();
                unit()
            }
            "finish-close" => {
                h.finish_close()?;
                unit()
            }
            "diagnostics" => {
                let d = h.diagnostics();
                Ok(
                    json!({"objects":d.objects,"references":d.references,"roots":d.roots,"calls":d.calls,"resources":d.resources,"tasks":d.tasks,"facets":d.facets,"closing":d.closing,"codeVersion":h.version()}),
                )
            }
            "drain" => Ok(json!(
                h.drain().into_iter().map(action_value).collect::<Vec<_>>()
            )),
            #[cfg(feature = "views")]
            "restrict" => {
                h.restrict(
                    id("reference")?,
                    strings(request, "methods")?.into_iter().collect(),
                )?;
                unit()
            }
            #[cfg(feature = "hooks")]
            "contribute" => {
                h.contribute(Contribution {
                    facet: id("facet")?,
                    target: text(request, "target")?.into(),
                    hook: text(request, "hook")?.into(),
                    operation: id("operation")?,
                    priority: request["priority"]
                        .as_i64()
                        .unwrap_or(0)
                        .try_into()
                        .map_err(|_| Error::invalid("priority", "priority exceeds i32"))?,
                    ordinal: id("ordinal")?,
                })?;
                unit()
            }
            #[cfg(feature = "hooks")]
            "hook-plan" => Ok(json!(&*h.hook_plan(
                id("root")?,
                text(request, "target")?,
                text(request, "hook")?
            )?)),
            #[cfg(feature = "subscriptions")]
            "listen" => Ok(json!(h.listen(
                id("scope")?,
                optional_number(request, "previous")?,
                text(request, "name")?.into(),
                id("source")?,
                optional_number(request, "sizing")?.unwrap_or(0),
                id("operation")?,
                id("maxPending")? as usize,
                optional_number(request, "maxBytes")?.map(|value| value as usize),
                request["coalesce"].as_bool().unwrap_or(false)
            )?)),
            #[cfg(feature = "subscriptions")]
            "subscribe" => Ok(json!(h.subscribe(
                owner(request)?,
                id("source")?,
                id("operation")?,
                id("maxPending")? as usize,
                optional_number(request, "maxBytes")?.map(|v| v as usize),
                request["coalesce"].as_bool().unwrap_or(false)
            )?)),
            #[cfg(feature = "subscriptions")]
            "enqueue" => {
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
            "next-delivery" => Ok(h
                .next_delivery(id("subscription")?)?
                .map_or(Value::Null, |d| json!(d.payload))),
            #[cfg(feature = "subscriptions")]
            "finish-delivery" => {
                h.finish_delivery(id("subscription")?)?;
                unit()
            }
            #[cfg(feature = "subscriptions")]
            "unsubscribe" => {
                h.unsubscribe(id("subscription")?)?;
                unit()
            }
            _ => Err(Error::new(
                Code::Unsupported,
                "operation",
                format!("operation {op} is unavailable in this build"),
            )),
        }
    }
}

fn step_value(step: Step) -> Value {
    match step {
        Step::Wait { deadline } => {
            json!({"type":"wait", "deadline":deadline.map(|deadline|deadline.to_string())})
        }
        Step::Prepared => json!({"type":"prepared"}),
        Step::Complete => json!({"type":"complete"}),
        Step::Failed(error) => {
            json!({"type":"failed","error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}})
        }
        Step::Execute(effects) => {
            json!({"type":"execute","effects":effects.into_iter().map(effect_value).collect::<Vec<_>>()})
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
        } => json!({"type":"stop","scope":scope,"keepListeners":keep_listeners}),
        Effect::Open { resource } => json!({"type":"open","resource":resource}),
        Effect::Close { resource } => json!({"type":"close","resource":resource}),
        Effect::Restore { previous, resource } => {
            json!({"type":"restore","previous":previous,"resource":resource})
        }
        Effect::Prepare { scope, previous } => {
            json!({"type":"prepare","scope":scope,"previous":previous})
        }
        Effect::Validate { scope, value } => json!({"type":"validate","scope":scope,"value":value}),
        Effect::Initialize { scope } => json!({"type":"initialize","scope":scope}),
        Effect::Migrate {
            scope,
            value,
            version,
        } => json!({"type":"migrate","scope":scope,"value":value,"version":version}),
        Effect::Adopt { replacements } => {
            json!({"type":"adopt","replacements":replacements.into_iter().map(|pair|json!({"previous":pair.previous,"next":pair.next})).collect::<Vec<_>>()})
        }
        Effect::Start { scope } => json!({"type":"start","scope":scope}),
        Effect::Drop { scope } => json!({"type":"drop","scope":scope}),
    }
}

fn parse_code(code: &str) -> Code {
    match code {
        "unavailable" => Code::Unavailable,
        "released" => Code::Released,
        "gone" => Code::Gone,
        "expired" => Code::Expired,
        "cancelled" => Code::Cancelled,
        "forbidden" => Code::Forbidden,
        "conflict" => Code::Conflict,
        "unsupported" => Code::Unsupported,
        "closed" => Code::Closed,
        "exhausted" => Code::Exhausted,
        _ => Code::Invalid,
    }
}

fn action_value(action: Action) -> Value {
    match action {
        Action::Execute { program, effects } => {
            json!({"type":"execute","id":program,"effects":effects.into_iter().map(effect_value).collect::<Vec<_>>()})
        }
        Action::CancelEffects { program, effects } => {
            json!({"type":"cancel-effects","id":program,"effects":effects.into_iter().map(effect_value).collect::<Vec<_>>()})
        }
        Action::ProgramPrepared(id) => json!({"type":"program-prepared","id":id}),
        Action::ProgramComplete { program, error } => {
            json!({"type":"program-complete","id":program,"error":error.map(error_value)})
        }
        Action::DisposeObject(id) => json!({"type":"dispose","id":id}),
        Action::CloseResource(id) => json!({"type":"close-resource","id":id}),
        Action::DropValue(id) => json!({"type":"drop-value","id":id}),
        Action::CancelRoot(id) => json!({"type":"cancel-root","id":id}),
        Action::CancelTask(id) => json!({"type":"cancel-task","id":id}),
        Action::RetireFacet(id) => json!({"type":"retire-facet","id":id}),
        Action::StateChanged { object, revision } => {
            json!({"type":"state","id":object,"revision":revision.to_string()})
        }
        #[cfg(feature = "subscriptions")]
        Action::SubscriptionFailed(id) => json!({"type":"subscription-failed","id":id}),
        #[cfg(feature = "subscriptions")]
        Action::DetachSubscription(id) => json!({"type":"detach-subscription","id":id}),
        #[cfg(feature = "subscriptions")]
        Action::Deliver {
            subscription,
            payload,
            operation,
        } => json!({"type":"deliver","id":subscription,"payload":payload,"operation":operation}),
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
        "services":f.declaration.services.iter().map(|s|json!({"name":s.name,"methods":s.methods,"lifetime":if s.lifetime==Lifetime::Host{"host"}else{"referenced"},"linger":s.linger_ms})).collect::<Vec<_>>(),"hooks":f.declaration.hooks,
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
                    _ => return Err(Error::invalid("lifetime", "unknown lifetime")),
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
                            .ok_or_else(|| Error::invalid("requires", "methods must be an array"))?
                            .iter()
                            .map(|v| {
                                v.as_str().map(String::from).ok_or_else(|| {
                                    Error::invalid("requires", "method must be text")
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
        Some("facet") => Ok(Owner::Facet(number(&v["owner"], "id")?)),
        Some("object") => Ok(Owner::Object(number(&v["owner"], "id")?)),
        _ => Err(Error::invalid("owner", "invalid owner")),
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| Error::invalid("metadata", format!("{key} must be text")))
}
fn number(v: &Value, key: &str) -> Result<Id> {
    v[key]
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| Error::invalid("metadata", format!("{key} must be a u32")))
}
fn optional_number(v: &Value, key: &str) -> Result<Option<Id>> {
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
                "metadata",
                format!("{key} must be an unsigned revision or clock"),
            )
        })
}
fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v[key]
        .as_array()
        .ok_or_else(|| Error::invalid("metadata", format!("{key} must be an array")))
}
fn strings(v: &Value, key: &str) -> Result<Vec<String>> {
    array(v, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| Error::invalid("metadata", "expected text"))
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
fn numbers(v: &Value, key: &str) -> Result<Vec<Id>> {
    array(v, key)?
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| n.try_into().ok())
                .ok_or_else(|| Error::invalid("metadata", "expected u32"))
        })
        .collect()
}
fn numbers_optional(v: &Value, key: &str) -> Result<Vec<Id>> {
    if v.get(key).is_none() {
        Ok(Vec::new())
    } else {
        numbers(v, key)
    }
}
