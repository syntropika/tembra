//! Decode metadata commands. The core owns every policy decision.

use super::decode::*;
use super::encode::*;
use super::*;

mod context;
mod lifecycle;

impl Kernel {
    /// Dispatch a validated operation name to the Rust kernel.
    pub fn execute(&self, request: &Value) -> Result<Value> {
        let name = text(request, "op")?;
        let op = KernelOp::parse(name).ok_or_else(|| unavailable(name))?;
        let h = &self.host;
        let id = |key| identity(request, key);
        let unit = || Ok(Value::Null);
        match op {
            KernelOp::ContextDerive | KernelOp::ContextValue | KernelOp::ContextStats => {
                self.execute_context(request, op)
            }
            KernelOp::Allocate => Ok(json!(h.allocate_handle()?)),
            KernelOp::Features => Ok(json!({
                "hooks": cfg!(feature="hooks"),
                "subscriptions": cfg!(feature="subscriptions"),
                "views": cfg!(feature="views"),
                "telemetry": cfg!(feature="telemetry"),
            })),
            KernelOp::Prepare => {
                let orbs = array(request, "orbs")?
                    .iter()
                    .map(parse_orb)
                    .collect::<Result<Vec<_>>>()?;
                let remove = strings_optional(request, "remove")?;
                let reload = h.prepare(orbs, remove)?;
                Ok(json!({
                    "id": reload.id,
                    "codeVersion": reload.code_version,
                    "orbs": reload.orbs.iter().map(orb_value).collect::<Vec<_>>(),
                    "objects": reload.objects,
                    "providers": reload.providers,
                }))
            }
            KernelOp::Gate => Ok(json!(h.gate(id("ticket")?, &numbers(request, "objects")?)?)),
            KernelOp::GateOrbs => Ok(json!(
                h.gate_orbs(id("ticket")?, &numbers(request, "owners")?)?
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
            KernelOp::Orbs => Ok(json!(
                h.installed_orbs().iter().map(orb_value).collect::<Vec<_>>()
            )),
            KernelOp::Orb => Ok(orb_value(&h.orb(id("id")?)?)),
            KernelOp::OrbNamed => {
                let name = text(request, "name")?;
                let orb = match optional_identity(request, "ticket")? {
                    Some(ticket) => h.prepared_orb(ticket, name)?,
                    None => h.orb_named(name)?,
                };
                Ok(orb_value(&orb))
            }
            KernelOp::NextDeadline => Ok(json!(h.next_deadline())),
            KernelOp::Address => {
                let a = h.object_address(id("object")?)?;
                Ok(json!({
                    "service": a.service,
                    "key": a.key,
                    "principal": a.principal,
                }))
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
                Ok(json!({
                    "reference": result.reference,
                    "object": result.object,
                    "orb": result.orb,
                    "codeVersion": result.code_version,
                    "activate": result.activate,
                }))
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
            KernelOp::BeginCall => Ok(invocation_value(&h.begin_call_with_context(
                id("reference")?,
                id("root")?,
                text(request, "method")?,
                optional_identity(request, "context")?.unwrap_or(0),
            )?)),
            KernelOp::BeginOperation => Ok(invocation_value(&h.begin_operation_with_context(
                id("orb")?,
                id("root")?,
                id("operation")?,
                optional_identity(request, "object")?,
                optional_identity(request, "context")?.unwrap_or(0),
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
                Ok(json!({ "id": r.id, "value": r.value }))
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
                Ok(json!({
                    "objects": d.objects,
                    "references": d.references,
                    "roots": d.roots,
                    "calls": d.calls,
                    "resources": d.resources,
                    "tasks": d.tasks,
                    "orbs": d.orbs,
                    "closing": d.closing,
                    "codeVersion": h.version(),
                }))
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
                    orb: id("orb")?,
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
            _ => self.execute_lifecycle(request, op),
        }
    }
}

/// Reject operations absent from this build's protocol implementation.
fn unavailable(name: &str) -> Error {
    Error::new(
        Code::Unsupported,
        Reason::Operation,
        format!("operation {name} is unavailable in this build"),
    )
}
