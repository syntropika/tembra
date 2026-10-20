//! Exact-engine borrowed evaluator. Its scope cannot escape the synchronous closure.
use deno_core::v8;
use serde_json::Value;
use tembra_core::{Result, RuntimeError};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallKind {
    Value,
    Draft,
    Void,
}

use std::any::Any;
type Function = v8::Global<v8::Function>;
pub(crate) trait Action {
    fn run(&mut self, scope: &mut v8::PinScope) -> Result<Box<dyn Any>>;
}
type Eval<'a> = &'a mut (dyn FnMut(&mut dyn Action) -> Result<Box<dyn Any>> + 'a);
struct Slot<'a> {
    id: u64,
    evaluator: Eval<'a>,
}
impl<'a, 'b: 'a> scoped_tls_hkt::ReborrowMut<'a> for Slot<'b> {
    type Result = Slot<'a>;
    fn reborrow_mut(&'a mut self) -> Slot<'a> {
        Slot {
            id: self.id,
            evaluator: &mut *self.evaluator,
        }
    }
}
scoped_tls_hkt::scoped_thread_local!(static mut CURRENT:for<'a> Slot<'a>);
pub(crate) fn is_active() -> bool {
    CURRENT.is_set()
}
pub(crate) fn within<R>(id: u64, evaluator: Eval<'_>, body: impl FnOnce() -> R) -> R {
    CURRENT.set(Slot { id, evaluator }, body)
}
struct Once<F>(Option<F>);
impl<F, R> Action for Once<F>
where
    F: for<'s, 'i> FnOnce(&mut v8::PinScope<'s, 'i>) -> Result<R>,
    R: 'static,
{
    fn run(&mut self, scope: &mut v8::PinScope) -> Result<Box<dyn Any>> {
        self.0
            .take()
            .ok_or_else(|| RuntimeError::invalid("scope_action", "scope action already consumed"))?(
            scope,
        )
        .map(|value| Box::new(value) as Box<dyn Any>)
    }
}
pub(crate) fn evaluate<R: 'static>(
    id: u64,
    body: impl for<'s, 'i> FnOnce(&mut v8::PinScope<'s, 'i>) -> Result<R>,
) -> Option<Result<R>> {
    if !CURRENT.is_set() {
        return None;
    }
    Some(CURRENT.with(
        |Slot {
             id: active,
             evaluator,
         }| {
            if active != id {
                return Err(RuntimeError::invalid(
                    "engine_scope",
                    "active evaluator belongs to another runtime",
                ));
            }
            evaluator(&mut Once(Some(body)))?
                .downcast::<R>()
                .map(|value| *value)
                .map_err(|_| RuntimeError::invalid("scope_action", "scope result type mismatch"))
        },
    ))
}
pub(crate) fn current(
    engine: &crate::engine::Engine,
    function: &Function,
    args: Vec<Value>,
    kind: CallKind,
) -> Option<Result<Value>> {
    evaluate(engine.id, |scope| {
        call(scope, function, args, kind, Some(engine))
    })
}
pub(crate) fn call(
    scope: &mut v8::PinScope,
    function: &Function,
    arguments: Vec<Value>,
    kind: CallKind,
    engine: Option<&crate::engine::Engine>,
) -> Result<Value> {
    v8::tc_scope!(let scope, scope);
    let arguments = arguments
        .into_iter()
        .map(|value| {
            deno_core::serde_v8::to_v8(scope, value)
                .map_err(|error| RuntimeError::invalid("javascript_value", error.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let function = v8::Local::new(scope, function);
    let receiver = v8::undefined(scope).into();
    let value = function.call(scope, receiver, &arguments).ok_or_else(|| {
        scope.exception().map_or_else(
            || RuntimeError::invalid("javascript", "JavaScript callback threw"),
            |exception| {
                crate::engine::promise_error(deno_core::error::JsError::from_v8_exception(
                    scope, exception,
                ))
            },
        )
    })?;
    if let Some(engine) = engine {
        crate::binding::helper(engine, scope, "synchronous", &[value])?;
    } else if value.is_promise() {
        return Err(RuntimeError::invalid(
            "async_declaration",
            "callback must be synchronous",
        ));
    }
    if kind == CallKind::Void {
        return Ok(Value::Null);
    }
    let value = if kind == CallKind::Draft {
        if !value.is_undefined() {
            return Err(RuntimeError::invalid(
                "change_return",
                "change callback must return undefined",
            ));
        }
        let engine = engine
            .ok_or_else(|| RuntimeError::invalid("engine_scope", "missing draft validator"))?;
        crate::binding::helper(engine, scope, "draftValue", &[arguments[0]])?
    } else if let Some(engine) = engine {
        crate::binding::helper(engine, scope, "portable", &[value])?
    } else {
        value
    };
    deno_core::serde_v8::from_v8(scope, value)
        .map_err(|error| RuntimeError::invalid("javascript_value", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_action_restores_nested_identity_after_unwind() {
        assert!(!CURRENT.is_set());
        let mut outer = |_: &mut dyn Action| Ok(Box::new(Value::Null) as Box<dyn Any>);
        within(1, &mut outer, || {
            CURRENT.with(|Slot { id, .. }| {
                assert_eq!(id, 1);
                assert!(!CURRENT.is_set());
                let mut inner = |_: &mut dyn Action| Ok(Box::new(Value::Null) as Box<dyn Any>);
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    within(2, &mut inner, || {
                        CURRENT.with(|Slot { id, .. }| assert_eq!(id, 2));
                        panic!("test unwind");
                    })
                }));
                assert!(panic.is_err());
                assert!(!CURRENT.is_set());
            });
            CURRENT.with(|Slot { id, .. }| assert_eq!(id, 1));
        });
        assert!(!CURRENT.is_set());
    }
}
