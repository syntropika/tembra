//! Decode lifecycle requests without running callbacks.

use super::*;

impl Kernel {
    /// Decode lifecycle metadata and dispatch it to the kernel.
    pub(super) fn execute_lifecycle(&self, request: &Value, op: KernelOp) -> Result<Value> {
        let h = &self.host;
        let id = |key| identity(request, key);
        let unit = || Ok(Value::Null);
        match op {
            KernelOp::ScopeRegister => {
                h.register_scope(Scope {
                    id: id("scope")?,
                    owner: owner(request)?,
                    orb: id("orb")?,
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
            KernelOp::ScopesForOrb => Ok(json!(h.scopes_for_orb(id("orb")?))),
            KernelOp::ScopeAddResource => {
                h.add_scope_resource(id("scope")?, id("resource")?)?;
                unit()
            }
            KernelOp::ScopeActivate => Ok(json!(h.activate_scope(id("scope")?)?)),
            KernelOp::ScopesCleanup => Ok(json!(h.cleanup_scopes(
                numbers(request, "scopes")?,
                optional_identity(request, "object")?
            )?)),
            KernelOp::ScopesReloadRegistered | KernelOp::ScopesReload => {
                let ticket = id("ticket")?;
                let deadline = wide(request, "deadline")?;
                let pairs = if op == KernelOp::ScopesReload {
                    Some(
                        array(request, "replacements")?
                            .iter()
                            .map(|pair| {
                                Ok(Replacement {
                                    previous: optional_identity(pair, "previous")?,
                                    next: optional_identity(pair, "next")?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                    )
                } else {
                    None
                };
                // Refresh host-relative monotonic time only after parsing the complete request.
                // Callers that already tick explicitly may omit this field.
                if request.get("now").is_some() {
                    h.tick(wide(request, "now")?)?;
                }
                Ok(json!(match pairs {
                    Some(pairs) => h.reload_scopes(ticket, pairs, deadline)?,
                    None => h.reload_registered_scopes(ticket, deadline)?,
                }))
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
            _ => Err(unavailable(op.as_str())),
        }
    }
}
