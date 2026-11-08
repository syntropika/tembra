//! Route scope-bound operations without transferring host callbacks into the guest.
use super::*;
use futures::FutureExt;

impl Environment {
    /// Resolve immediate operations against the current host-issued scope only.
    pub(super) fn sync(&self, scope: &Scope, packet: &str) -> Result<Value> {
        let request: Value = serde_json::from_str(packet).map_err(failure)?;
        let handles = self.selected_handles(&request)?;
        let operation = text(&request, "operation")?;
        if operation == "console" {
            return self.session.log(packet);
        }
        if matches!(scope, Scope::Validation) && operation != "state.read" {
            return Err(failure(
                "reload validators may only read their retained state",
            ));
        }
        if ![
            "signal.cancelled",
            "state.read",
            "resource.read",
            "reference.state",
            "reference.status",
            "reference.release",
        ]
        .contains(&operation)
        {
            if let Scope::Call(context) = scope {
                context.check()?;
            }
            if let Scope::Open(signal) = scope {
                signal.check()?;
            }
        }
        let id = || number(&request, "id");
        match operation {
            "address" => {
                let declaration = Self::declaration(scope)?;
                if self.service.is_none() {
                    return Ok(json!({"key":[],"principal":"host"}));
                }
                let address = declaration.address()?;
                Ok(json!({"key":address.key,"principal":address.principal}))
            }
            "orb" => Ok(json!(Self::declaration(scope)?.orb::<Environment>()?.id)),
            "state.declare" => {
                let state = Self::declaration(scope)?.state(
                    request["initial"].clone(),
                    number(&request, "version")?,
                    |_| Ok(()),
                )?;
                if !request["validate"].is_null() {
                    let callback = number(&request, "validate")?;
                    let check_state = state.clone();
                    let environment = self.clone();
                    Self::declaration(scope)?.validate_on_reload(move || {
                        environment.session.blocking("declare",json!({"kind":"validate","callback":callback,"value":*check_state.value()?}).to_string(),|packet,_| environment.sync(&Scope::Validation,packet))?;
                        Ok(())
                    })?;
                }
                let id = self.allocate()?;
                self.handles.states.borrow_mut().insert(id, state);
                Ok(json!(id))
            }
            "state.read" => {
                let state = handles
                    .states
                    .borrow()
                    .get(&id()?)
                    .cloned()
                    .ok_or_else(|| failure("unknown state handle"))?;
                let snapshot = state.snapshot()?;
                Ok(
                    json!({"value":*state.value()?,"revision":snapshot.revision.to_string(),"version":snapshot.version}),
                )
            }
            "state.update" => {
                let context = Self::context(scope)?;
                let state = handles
                    .states
                    .borrow()
                    .get(&id()?)
                    .cloned()
                    .ok_or_else(|| failure("unknown state handle"))?;
                let value = state.update(context, |_| Ok(request["value"].clone()))?;
                let snapshot = state.snapshot()?;
                Ok(
                    json!({"value":*value,"revision":snapshot.revision.to_string(),"version":snapshot.version}),
                )
            }
            "reference.declare" => {
                let requested = text(&request, "service")?;
                let service = self
                    .imports
                    .get(requested)
                    .map(String::as_str)
                    .ok_or_else(|| failure("reference is not a selected WIT Orb import"))?;
                let reference = Self::declaration(scope)?.get(
                    service,
                    serde_json::from_value(request["key"].clone()).map_err(failure)?,
                )?;
                let id = self.allocate()?;
                self.handles
                    .references
                    .borrow_mut()
                    .insert(id, (reference, requested.to_owned()));
                Ok(json!(id))
            }
            "reference.release" => {
                handles
                    .references
                    .borrow()
                    .get(&id()?)
                    .ok_or_else(|| failure("unknown reference handle"))?
                    .0
                    .release()?;
                Ok(Value::Null)
            }
            "reference.status" => Ok(json!(
                handles
                    .references
                    .borrow()
                    .get(&id()?)
                    .ok_or_else(|| failure("unknown reference handle"))?
                    .0
                    .status()?
                    .as_str()
            )),
            "reference.state" => {
                let (snapshot, value) = handles
                    .references
                    .borrow()
                    .get(&id()?)
                    .ok_or_else(|| failure("unknown reference handle"))?
                    .0
                    .state_snapshot()?;
                Ok(
                    json!({"value":value,"revision":snapshot.revision.to_string(),"version":snapshot.version}),
                )
            }
            "resource.declare" => self.resource(scope, &request),
            "resource.read" => Ok(json!(
                *handles
                    .resources
                    .borrow()
                    .get(&id()?)
                    .ok_or_else(|| failure("unknown resource handle"))?
                    .value()?
            )),
            "context.check" => {
                Self::context(scope)?;
                Ok(Value::Null)
            }
            "context.principal" => Ok(json!(Self::context(scope)?.principal()?)),
            "context.cancel" => {
                Self::context(scope)?.cancel()?;
                Ok(Value::Null)
            }
            "filesystem" | "network" => match scope {
                Scope::Call(_) | Scope::Open(_) => Ok(Value::Null),
                Scope::Declaration(_) | Scope::Validation => Err(failure(
                    "filesystem I/O must run in a resource opener or an admitted call",
                )),
            },
            "signal.cancelled" => Ok(json!(match scope {
                Scope::Call(context) => context.signal().is_cancelled(),
                Scope::Open(signal) => signal.is_cancelled(),
                Scope::Declaration(_) | Scope::Validation => false,
            })),
            _ => Err(failure(format!(
                "unknown or forbidden host operation {operation}"
            ))),
        }
    }

    /// Declare open and close callbacks without letting the guest acknowledge settlement itself.
    fn resource(&self, scope: &Scope, request: &Value) -> Result<Value> {
        let declaration = Self::declaration(scope)?;
        let open = number(request, "open")?;
        let close = number(request, "close")?;
        let dependencies = request["dependencies"]
            .as_array()
            .ok_or_else(|| failure("resource dependencies must be an array"))?
            .iter()
            .map(|dependency| {
                self.selected_handles(dependency)?
                    .resources
                    .borrow()
                    .get(&number(dependency, "id")?)
                    .map(Resource::key)
                    .ok_or_else(|| failure("unknown resource dependency"))
            })
            .collect::<Result<Vec<_>>>()?;
        let opening = self.clone();
        let closing = self.clone();
        let resource = declaration.resource_async_in_realm(
            text(request, "name")?,
            number(request, "version")?,
            &dependencies,
            self.handles.realm,
            move |signal| {
                async move {
                    let value = opening
                        .execute(
                            Scope::Open(signal),
                            json!({"kind":"callback","callback":open}),
                        )
                        .await?;
                    serde_json::from_value::<u32>(value).map_err(failure)
                }
                .boxed_local()
            },
            move |value, context| {
                async move {
                    closing
                        .execute(
                            Scope::Call(context),
                            json!({"kind":"callback","callback":close,"value":*value}),
                        )
                        .await?;
                    Ok(())
                }
                .boxed_local()
            },
        )?;
        let id = self.allocate()?;
        self.handles.resources.borrow_mut().insert(id, resource);
        Ok(json!(id))
    }

    /// Await portable references and cooperative timers under the active invocation's authority.
    pub(super) async fn asynchronous(&self, scope: &Scope, packet: &str) -> Result<Value> {
        let request: Value = serde_json::from_str(packet).map_err(failure)?;
        let handles = self.selected_handles(&request)?;
        match text(&request, "operation")? {
            "reference.ready" => {
                let (reference, _) = handles
                    .references
                    .borrow()
                    .get(&number(&request, "id")?)
                    .cloned()
                    .ok_or_else(|| failure("unknown reference handle"))?;
                reference.ready().await?;
                Ok(Value::Null)
            }
            "reference.call" => {
                let context = Self::context(scope)?;
                let (reference, binding) = handles
                    .references
                    .borrow()
                    .get(&number(&request, "id")?)
                    .cloned()
                    .ok_or_else(|| failure("unknown reference handle"))?;
                let mut method = String::new();
                for character in text(&request, "method")?.chars() {
                    if character.is_ascii_uppercase() {
                        method.push('-');
                        method.push(character.to_ascii_lowercase());
                    } else {
                        method.push(character);
                    }
                }
                self.contract
                    .validate_import_arguments(&binding, &method, &request["args"])
                    .map_err(failure)?;
                let value = reference
                    .call_value(context, &method, request["args"].clone())
                    .await?;
                self.contract
                    .validate_import_result(&binding, &method, &value)
                    .map_err(failure)?;
                Ok(value)
            }
            "wait" => {
                let milliseconds = number(&request, "milliseconds")?;
                if milliseconds > 5000 {
                    return Err(failure("timer exceeds the callback wait limit"));
                }
                let signal = match scope {
                    Scope::Call(context) => {
                        context.check()?;
                        Some(context.signal())
                    }
                    Scope::Open(signal) => Some(signal.clone()),
                    _ => None,
                };
                let Some(signal) = signal else {
                    return Err(failure("timer requires an active callback"));
                };
                signal.check()?;
                let wait = std::pin::pin!(futures_timer::Delay::new(
                    std::time::Duration::from_millis(milliseconds.into())
                ));
                let cancelled = std::pin::pin!(signal.cancelled());
                match futures::future::select(wait, cancelled).await {
                    futures::future::Either::Left(_) => Ok(Value::Null),
                    futures::future::Either::Right(_) => Err(failure("operation cancelled")),
                }
            }
            _ => Err(failure("unknown or forbidden asynchronous host operation")),
        }
    }
}

/// Require a string field before interpreting a host operation.
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| failure(format!("expected string {key}")))
}

/// Reject truncated, signed or non-integer guest handles and versions.
fn number(value: &Value, key: &str) -> Result<u32> {
    value[key]
        .as_u64()
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| failure(format!("expected u32 {key}")))
}
