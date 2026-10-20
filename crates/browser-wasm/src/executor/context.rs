use super::{Generation, handles::WasmBoundReference, support::*};
use crate::{
    boundary::failure,
    runtime::{ContextLease, WebHost},
};
use futures::FutureExt;
use js_sys::Promise;
use std::rc::{Rc, Weak};
use tembra_core::{
    CancellationToken, Context, FacetSignal, GetOptions, InvocationLease, InvocationOwner, Result,
    RuntimeError,
};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

pub(crate) struct IssuedContext {
    lease: ContextLease,
    context: Context,
    generation: Weak<Generation>,
    signal: CancellationToken,
}
impl IssuedContext {
    pub(crate) fn clone_wrapper(self: &Rc<Self>) -> WasmManagedContext {
        WasmManagedContext {
            inner: Rc::downgrade(self),
        }
    }
}

/// Opaque exact-context access. Saving a wrapper never extends the logical scope.
#[wasm_bindgen]
pub struct WasmManagedContext {
    inner: Weak<IssuedContext>,
}
impl WasmManagedContext {
    pub(crate) fn issue(
        generation: &Rc<Generation>,
        context: Context,
        invocation: Option<InvocationOwner>,
    ) -> Result<Rc<IssuedContext>> {
        let host = generation.host()?;
        context.check(host.host.id())?;
        let signal = context.cancellation().child();
        let expired = signal.clone();
        context.on_settle(move || {
            expired.cancel(RuntimeError::aborted("expired", "context scope settled"))
        })?;
        let lease = host.issue_bound_context(
            context.clone(),
            invocation,
            Some(generation.session.token.clone()),
        )?;
        Ok(Rc::new(IssuedContext {
            lease,
            context,
            generation: Rc::downgrade(generation),
            signal,
        }))
    }
    fn record(&self) -> Result<Rc<IssuedContext>> {
        self.inner
            .upgrade()
            .ok_or_else(|| RuntimeError::aborted("expired", "context scope settled"))
    }
}
#[wasm_bindgen]
impl WasmManagedContext {
    /// # Errors
    /// Returns the existing opaque token. Refuses after its physical observation releases the
    /// record.
    pub fn token(&self) -> std::result::Result<JsValue, JsValue> {
        Ok(self.record().map_err(failure)?.lease.token.clone())
    }
    /// # Errors
    /// Refuses expired, cancelled, foreign or unavailable Context/generation admission before
    /// effects.
    pub fn check(&self) -> std::result::Result<(), JsValue> {
        let record = self.record().map_err(failure)?;
        let generation = record
            .generation
            .upgrade()
            .ok_or_else(|| failure(error("generation", "execution generation ended")))?;
        record
            .context
            .check(generation.host().map_err(failure)?.host.id())
            .map_err(failure)
    }
    /// # Errors
    /// Refuses stale admission or invalid reason text; signals only this actual Context.
    pub fn cancel(&self, reason: JsValue) -> std::result::Result<(), JsValue> {
        self.check()?;
        self.record()
            .map_err(failure)?
            .context
            .cancel(string(&reason).map_err(failure)?);
        Ok(())
    }
    /// # Errors
    /// Refuses an ended record. The returned data is an observation, never principal authority.
    #[wasm_bindgen(js_name = principalJson)]
    pub fn principal_json(&self) -> std::result::Result<String, JsValue> {
        Ok(self
            .record()
            .map_err(failure)?
            .context
            .principal()
            .to_string())
    }
    /// # Errors
    /// Refuses an ended record; the observed epoch cannot be assigned by JavaScript.
    #[wasm_bindgen(js_name = codeVersion)]
    pub fn code_version(&self) -> std::result::Result<f64, JsValue> {
        Ok(self.record().map_err(failure)?.context.code_version() as f64)
    }
    /// # Errors
    /// Refuses an ended record. The returned capability only observes cancellation.
    pub fn signal(&self) -> std::result::Result<WasmSignal, JsValue> {
        Ok(WasmSignal::token(
            self.record().map_err(failure)?.signal.clone(),
        ))
    }
    /// # Errors
    /// Refuses invalid descriptor/input/Context admission; the Promise retains ordinary call-scoped
    /// acquisition ownership.
    pub fn get(
        &self,
        descriptor: JsValue,
        input: JsValue,
        state: bool,
    ) -> std::result::Result<Promise, JsValue> {
        self.check()?;
        let record = self.record().map_err(failure)?;
        let generation = record
            .generation
            .upgrade()
            .ok_or_else(|| failure(error("generation", "execution generation ended")))?;
        let contract = contract(&descriptor).map_err(failure)?;
        let input = strict_json(&input).map_err(failure)?;
        let context = record.context.clone();
        let (_, physical) = generation
            .host()
            .map_err(failure)?
            .call_context(&record.lease.token, Some(&generation.session.token))?;
        Ok(future_to_promise(async move {
            let _physical = physical;
            let reference = context
                .get(
                    contract,
                    input,
                    GetOptions {
                        state,
                        ..GetOptions::default()
                    },
                )
                .await
                .map_err(failure)?;
            Ok(WasmBoundReference::new(reference, &generation).into())
        }))
    }
}

#[derive(Clone)]
enum Signal {
    Token(CancellationToken),
    Facet(FacetSignal),
}
/// Read-only signal: no constructor or cancel method grants owner authority.
#[wasm_bindgen]
pub struct WasmSignal {
    inner: Signal,
}
impl WasmSignal {
    pub(crate) fn notification(&self) -> futures::future::LocalBoxFuture<'static, RuntimeError> {
        match &self.inner {
            Signal::Token(token) => token.cancelled(),
            Signal::Facet(signal) => signal.cancelled(),
        }
    }

    pub(crate) fn token(token: CancellationToken) -> Self {
        Self {
            inner: Signal::Token(token),
        }
    }
    pub(crate) fn facet(signal: FacetSignal) -> Self {
        Self {
            inner: Signal::Facet(signal),
        }
    }
}
#[wasm_bindgen]
impl WasmSignal {
    pub fn aborted(&self) -> bool {
        match &self.inner {
            Signal::Token(v) => v.is_cancelled(),
            Signal::Facet(v) => v.is_cancelled(),
        }
    }
    #[wasm_bindgen(js_name = reasonJson)]
    pub fn reason_json(&self) -> String {
        let reason = match &self.inner {
            Signal::Token(v) => v.check().err(),
            Signal::Facet(v) if v.is_cancelled() => v.cancelled().now_or_never(),
            Signal::Facet(_) => None,
        };
        serde_json::to_string(&reason).unwrap_or_else(|_| "null".into())
    }
    pub fn cancelled(&self) -> Promise {
        let signal = self.inner.clone();
        future_to_promise(async move {
            match signal {
                Signal::Token(v) => {
                    v.cancelled().await;
                }
                Signal::Facet(v) => {
                    v.cancelled().await;
                }
            }
            Ok(JsValue::UNDEFINED)
        })
    }
}

pub(crate) fn checked_context(
    host: &Weak<WebHost>,
    generation: &Weak<Generation>,
    token: &JsValue,
) -> Result<Context> {
    let host = host
        .upgrade()
        .ok_or_else(|| RuntimeError::unavailable("closed", "browser Host ended"))?;
    let generation = super::handles::generation(generation)?;
    host.context_for(token, Some(&generation.session.token))
        .map_err(js_error)
}

pub(crate) fn call_context(
    host: &Weak<WebHost>,
    generation: &Weak<Generation>,
    token: &JsValue,
) -> Result<(Context, Option<InvocationLease>)> {
    let host = host
        .upgrade()
        .ok_or_else(|| RuntimeError::unavailable("closed", "browser Host ended"))?;
    let generation = super::handles::generation(generation)?;
    host.call_context(token, Some(&generation.session.token))
        .map_err(js_error)
}
