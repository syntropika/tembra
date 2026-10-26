//! Transport typed metadata and owner-issued opaque handles.

use super::*;

impl Kernel {
    /// Decode context operations without interpreting owner resources.
    pub(super) fn execute_context(&self, request: &Value, op: KernelOp) -> Result<Value> {
        let h = &self.host;
        let id = |key| identity(request, key);
        match op {
            KernelOp::ContextStats => {
                let (live, peak, keys) = h.context_stats();
                Ok(json!({"live":live,"peak":peak,"keys":keys}))
            }
            KernelOp::ContextDerive | KernelOp::ContextValue => {
                let root = id("root")?;
                let frame = optional_identity(request, "context")?.unwrap_or(0);
                let key = context_contract(&request["key"])?;
                if let Some(call) = optional_identity(request, "call")?
                    && h.invocation(call)?.root != root
                {
                    return Err(Error::invalid(Reason::Context, "foreign invocation"));
                }
                if op == KernelOp::ContextValue {
                    return Ok(match h.context_value(root, frame, &key)? {
                        None => Value::Null,
                        Some(ContextValue::Text(value)) => json!({"kind":"text","value":&*value}),
                        Some(ContextValue::Integer(value)) => {
                            json!({"kind":"integer","value":value})
                        }
                        Some(ContextValue::Boolean(value)) => {
                            json!({"kind":"boolean","value":value})
                        }
                        Some(ContextValue::Opaque(_)) => json!({"kind":"opaque","source":frame}),
                    });
                }
                let value = match key.kind {
                    ContextKind::Text => ContextValue::Text(text(request, "value")?.into()),
                    ContextKind::Integer => {
                        ContextValue::Integer(request["value"].as_i64().ok_or_else(|| {
                            Error::invalid(Reason::Context, "expected a safe integer")
                        })?)
                    }
                    ContextKind::Boolean => ContextValue::Boolean(
                        request["value"]
                            .as_bool()
                            .ok_or_else(|| Error::invalid(Reason::Context, "expected a boolean"))?,
                    ),
                    ContextKind::Opaque => h
                        .context_value(root, id("source")?, &key)?
                        .filter(|value| matches!(value, ContextValue::Opaque(_)))
                        .ok_or_else(|| {
                            Error::invalid(
                                Reason::Context,
                                "opaque references must be issued by their owner",
                            )
                        })?,
                };
                Ok(json!(h.derive_context(
                    root,
                    frame,
                    optional_identity(request, "call")?,
                    key,
                    value
                )?))
            }
            _ => Err(unavailable(op.as_str())),
        }
    }
}

/// Decode a stable key identity and scalar kind.
fn context_contract(value: &Value) -> Result<ContextContract> {
    let kind = match text(value, "kind")? {
        "text" => ContextKind::Text,
        "integer" => ContextKind::Integer,
        "boolean" => ContextKind::Boolean,
        "opaque" => ContextKind::Opaque,
        _ => return Err(Error::invalid(Reason::Context, "unknown context kind")),
    };
    Ok(ContextContract {
        name: text(value, "name")?.into(),
        version: number(value, "version")?,
        kind,
    })
}
