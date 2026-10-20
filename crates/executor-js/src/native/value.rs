use crate::engine::invalid;
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::Value;
use std::{
    any::Any,
    collections::BTreeSet,
    rc::{Rc, Weak},
};
use tembra_core::{Result, RuntimeError};

/// Explicit adapter view; ordinary core methods retain their real Rust types.
#[derive(Clone)]
pub struct NativeValue {
    pub(crate) inner: NativeData,
}
#[derive(Clone)]
pub(crate) enum NativeData {
    Absent,
    Portable(Value),
    Capability(Rc<dyn Any>),
    Record(NativeRecord),
    Callable(NativeCallable),
}
/// Immutable local fields, including explicit absence and issued native capabilities.
/// The record envelope is structural across foreign roundtrips; object identity is not
/// promised. Each issued capability or callable field retains its own checked provenance.
///
/// ```
/// use tembra_executor_js::{NativeRecord, NativeValue};
/// use serde_json::json;
/// let empty = NativeRecord::new([]).unwrap();
/// assert_eq!(empty.entries().len(), 0);
/// let fields = NativeRecord::new([
///     ("missing".into(), NativeValue::absent()),
///     ("flag".into(), NativeValue::portable(json!(false))),
/// ]).unwrap();
/// assert!(fields.get("missing").unwrap().is_absent());
/// assert_eq!(fields.get("flag").unwrap().clone().into_portable().unwrap(), json!(false));
/// let duplicate = NativeRecord::new([
///     ("key".into(), NativeValue::absent()), ("key".into(), NativeValue::absent()),
/// ]);
/// assert_eq!(duplicate.err().unwrap().reason, "native_record");
/// assert_eq!(fields.entries().len(), 2);
/// ```
#[derive(Clone)]
pub struct NativeRecord {
    fields: Rc<Vec<(String, NativeValue)>>,
}
impl NativeRecord {
    /// Copies fields in the supplied order without executing callbacks.
    /// # Errors
    /// Refuses duplicate field names instead of replacing an earlier value.
    pub fn new(fields: impl IntoIterator<Item = (String, NativeValue)>) -> Result<Self> {
        let mut names = BTreeSet::new();
        let mut entries = Vec::new();
        for (name, value) in fields {
            if !names.insert(name.clone()) {
                return Err(invalid("native_record", "duplicate native record field"));
            }
            entries.push((name, value));
        }
        Ok(Self {
            fields: Rc::new(entries),
        })
    }
    pub fn get(&self, key: &str) -> Option<&NativeValue> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&str, &NativeValue)> {
        self.fields.iter().map(|(key, value)| (key.as_str(), value))
    }
}
/// Owns the raw argument and its engine until explicitly decoded once.
/// This is a deferred reference view, not a snapshot: JS mutations before the first
/// Driver poll are visible to decoding. It does not clone nominal native values.
pub type NativeDecoder = Box<dyn FnOnce() -> Result<NativeValue>>;
/// A simple callback; decoding happens when its owned future is first polled.
pub type NativeCallableFn = Rc<dyn Fn(NativeValue) -> LocalBoxFuture<'static, Result<NativeValue>>>;
/// Advanced preparation runs after bounded ingress admission, before decoding or physical work.
pub type DeferredNativeCallableFn = Rc<dyn Fn(NativeDecoder) -> NativeCall>;
/// Trusted local callback definition. Issuing it to JS binds actual receiver provenance.
#[derive(Clone)]
pub struct NativeCallable {
    callback: DeferredNativeCallableFn,
    issued: Option<Weak<crate::slots::Capability>>,
}
impl NativeCallable {
    /// Defers argument conversion and the callback until Driver-owned work is polled.
    /// Issued JS functions carry the actual Host, module and entered receiver authority.
    pub fn new(callback: NativeCallableFn) -> Self {
        Self::deferred(Rc::new(move |decode| {
            let callback = callback.clone();
            NativeCall::new(async move { callback(decode()?).await }.boxed_local())
        }))
    }
    /// Runs preparation synchronously after checked provenance and bounded ingress admission.
    /// Preparation may reserve a continuation or perform checked synchronous core work using
    /// the current evaluator. It must leave raw decoding and user future polling to returned
    /// owned work; recursively driving the executor is unsupported. Preparation effects are
    /// not rolled back on later refusal. Unwind refunds ingress credit.
    pub fn deferred(callback: DeferredNativeCallableFn) -> Self {
        Self {
            callback,
            issued: None,
        }
    }
    pub(crate) fn issued(&self, cap: &Rc<crate::slots::Capability>) -> Self {
        Self {
            callback: self.callback.clone(),
            issued: Some(Rc::downgrade(cap)),
        }
    }
    pub(crate) fn issued_cap(&self) -> Option<Result<Rc<crate::slots::Capability>>> {
        self.issued
            .as_ref()
            .map(|cap| cap.upgrade().ok_or_else(crate::slots::expired))
    }
    pub(crate) fn invoke(&self, input: NativeDecoder) -> NativeCall {
        (self.callback)(input)
    }
}
/// Optional scheduling and projection protocol for an explicitly deferred local callback.
/// Admission runs under the current safe evaluator and must not poll/decode user work.
/// Refusal and completion run outside engine borrows; their effects are not rolled back.
/// Unwind panics are reported as foreign callback failures, not process-abort recovery.
pub trait NativeCallControl: 'static {
    /// Acknowledges accepted scheduling before a foreign handler can settle.
    /// # Errors
    /// Reports scope/protocol refusal without polling the returned future.
    fn admit(&mut self) -> Result<()> {
        Ok(())
    }
    /// Records a prepared backend refusal separately from final projection failure.
    fn refuse(&mut self, _cause: RuntimeError) {}
    /// Finishes only output/rejection preparation. A prepared rejected Promise reports Ok.
    fn complete(self: Box<Self>, projection: Result<()>);
}
struct CompletionFn(Option<Box<dyn FnOnce(Result<()>)>>);
impl NativeCallControl for CompletionFn {
    fn complete(mut self: Box<Self>, result: Result<()>) {
        if let Some(callback) = self.0.take() {
            callback(result);
        }
    }
}
/// An owned callback outcome plus optional final projection acknowledgement.
/// Dropping the JS observer never drops the callback job. The completion reports only
/// checked output/rejection preparation, not whether JS observed its result.
pub struct NativeCall {
    future: LocalBoxFuture<'static, Result<NativeValue>>,
    completion: CompletionGuard,
}
impl NativeCall {
    /// Creates work without a projection observer. Once admitted, the Driver retains it
    /// independently of the JS observer until settlement and final conversion.
    pub fn new(future: LocalBoxFuture<'static, Result<NativeValue>>) -> Self {
        Self {
            future,
            completion: CompletionGuard(None),
        }
    }
    /// Adds one final projection acknowledgment. A prepared rejected Promise reports Ok even
    /// when the callback outcome or backend refusal was Err; conversion or abandonment reports Err.
    /// Acknowledgment runs outside engine borrows and cannot undo callback effects.
    /// Unwind panics are contained and reported by an executing adapter; panic-abort and
    /// allocation failure cannot be recovered. Before admission, Drop attempts abandonment.
    pub fn with_completion(
        future: LocalBoxFuture<'static, Result<NativeValue>>,
        completed: Box<dyn FnOnce(Result<()>)>,
    ) -> Self {
        Self {
            future,
            completion: CompletionGuard(Some(Box::new(CompletionFn(Some(completed))))),
        }
    }
    /// Installs a scheduling/refusal protocol without executing it at construction.
    /// Scheduling acknowledgment runs after checked foreign ingress admission; Drop reports
    /// abandonment even if the value was never issued. This is local to this callable.
    pub fn with_control(
        future: LocalBoxFuture<'static, Result<NativeValue>>,
        control: Box<dyn NativeCallControl>,
    ) -> Self {
        Self {
            future,
            completion: CompletionGuard(Some(control)),
        }
    }
    pub(crate) fn admit(&mut self) -> Result<()> {
        self.completion.admit()
    }
    pub(crate) fn into_parts(
        self,
    ) -> (
        LocalBoxFuture<'static, Result<NativeValue>>,
        CompletionGuard,
    ) {
        (self.future, self.completion)
    }
}
pub(crate) struct CompletionGuard(Option<Box<dyn NativeCallControl>>);
impl CompletionGuard {
    fn admit(&mut self) -> Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.0.as_mut().map_or(Ok(()), |control| control.admit())
        }))
        .map_err(|_| {
            invalid(
                "foreign_callback_panic",
                "native scheduling admission panicked",
            )
        })?
    }
    pub(crate) fn refuse(&mut self, cause: RuntimeError) -> Result<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(control) = self.0.as_mut() {
                control.refuse(cause);
            }
        }))
        .map_err(|_| {
            invalid(
                "foreign_callback_panic",
                "native scheduling refusal panicked",
            )
        })
    }

    pub(crate) fn complete(mut self, result: Result<()>) -> Result<()> {
        if let Some(callback) = self.0.take() {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback.complete(result)))
                .map_err(|_| {
                    invalid(
                        "foreign_callback_panic",
                        "native projection completion panicked",
                    )
                })?;
        }
        Ok(())
    }
}
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback.complete(Err(RuntimeError::aborted(
                    "native_output_abandoned",
                    "native output preparation was abandoned",
                )))
            }));
        }
    }
}
impl NativeValue {
    pub fn absent() -> Self {
        Self {
            inner: NativeData::Absent,
        }
    }
    pub fn portable(value: Value) -> Self {
        Self {
            inner: NativeData::Portable(value),
        }
    }
    pub fn capability<T: 'static>(value: Rc<T>) -> Self {
        Self {
            inner: NativeData::Capability(value),
        }
    }
    pub fn record(value: NativeRecord) -> Self {
        Self {
            inner: NativeData::Record(value),
        }
    }
    pub fn callable(value: NativeCallable) -> Self {
        Self {
            inner: NativeData::Callable(value),
        }
    }
    pub fn is_absent(&self) -> bool {
        matches!(self.inner, NativeData::Absent)
    }
    /// Extracts the actual nominal Rc without JSON.
    /// # Errors
    /// Refuses a different view or Rust nominal type.
    pub fn downcast<T: 'static>(self) -> Result<Rc<T>> {
        match self.inner {
            NativeData::Capability(value) => value
                .downcast()
                .map_err(|_| invalid("native_type", "native capability type mismatch")),
            _ => Err(invalid("native_type", "expected native capability")),
        }
    }
    /// Extracts explicitly portable data, never converting native values to JSON.
    /// # Errors
    /// Refuses absence, records and capabilities/callables.
    pub fn into_portable(self) -> Result<Value> {
        match self.inner {
            NativeData::Portable(value) => Ok(value),
            _ => Err(invalid("native_type", "expected portable adapter value")),
        }
    }
    /// Projects an explicit record or a strict portable object into immutable fields.
    /// # Errors
    /// Refuses other views. Portable fields do not acquire native authority.
    pub fn into_record(self) -> Result<NativeRecord> {
        match self.inner {
            NativeData::Record(value) => Ok(value),
            NativeData::Portable(Value::Object(value)) => NativeRecord::new(
                value
                    .into_iter()
                    .map(|(key, value)| (key, Self::portable(value))),
            ),
            _ => Err(invalid("native_record", "expected native record")),
        }
    }
    /// Extracts an issued local callback definition.
    /// # Errors
    /// Refuses structural objects and other native views.
    pub fn into_callable(self) -> Result<NativeCallable> {
        match self.inner {
            NativeData::Callable(value) => Ok(value),
            _ => Err(invalid("native_callable", "expected issued callable")),
        }
    }
}
