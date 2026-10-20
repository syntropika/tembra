use std::{cell::RefCell, rc::Rc};

use futures::FutureExt;
use js_sys::{Function, Promise};
use serde_json::{Value, json};
use tembra_core::rpc::{CloseReason, Connection, ConnectionOptions, ConnectionStatus};
use tembra_core::{
    Context, GetOptions, Host, HostOptions, InvocationLease, InvocationOwner, RefStatus, Reference,
    RuntimeError, Shape,
};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::{JsFuture, future_to_promise};

use crate::{
    BrowserDriver, BrowserLinkOptions, WasmReadable, WasmSubscription,
    boundary::{contract_failure, failure, json as parse, text},
    intrinsics::fresh_context_token,
    websocket_link,
};

pub(crate) struct WebHost {
    pub host: Host,
    contexts: RefCell<Vec<ContextEntry>>,
    callback_errors: RefCell<Vec<RuntimeError>>,
}
#[derive(Clone)]
struct ContextEntry {
    token: JsValue,
    context: Context,
    invocation: Option<InvocationOwner>,
    session: Option<JsValue>,
}

impl WebHost {
    pub(crate) fn context(&self, token: &JsValue) -> Result<Context, JsValue> {
        self.context_for(token, None)
    }
    fn entry(&self, token: &JsValue, session: Option<&JsValue>) -> Result<ContextEntry, JsValue> {
        let entry = self
            .contexts
            .borrow()
            .iter()
            .find(|entry| entry.token == *token)
            .cloned()
            .ok_or_else(|| {
                failure(RuntimeError::invalid(
                    "foreign_ctx",
                    "context was not issued by this host",
                ))
            })?;
        if let (Some(expected), Some(issued)) = (session, entry.session.as_ref())
            && expected != issued
        {
            return Err(failure(RuntimeError::invalid(
                "foreign_runtime",
                "Context belongs to another execution session",
            )));
        }
        entry.context.check(self.host.id()).map_err(failure)?;
        Ok(entry)
    }
    pub(crate) fn context_for(
        &self,
        token: &JsValue,
        session: Option<&JsValue>,
    ) -> Result<Context, JsValue> {
        Ok(self.entry(token, session)?.context)
    }
    pub(crate) fn call_context(
        &self,
        token: &JsValue,
        session: Option<&JsValue>,
    ) -> Result<(Context, Option<InvocationLease>), JsValue> {
        let entry = self.entry(token, session)?;
        let physical = entry
            .invocation
            .as_ref()
            .map(|owner| owner.retain(&entry.context))
            .transpose()
            .map_err(failure)?;
        Ok((entry.context, physical))
    }

    fn remove_context(&self, token: &JsValue) {
        let removed = {
            let mut contexts = self.contexts.borrow_mut();
            contexts
                .iter()
                .position(|entry| entry.token == *token)
                .map(|index| contexts.swap_remove(index))
        };
        drop(removed);
    }

    pub(crate) fn issue_context(
        self: &Rc<Self>,
        context: Context,
    ) -> Result<ContextLease, RuntimeError> {
        self.issue_bound_context(context, None, None)
    }
    pub(crate) fn issue_bound_context(
        self: &Rc<Self>,
        context: Context,
        invocation: Option<InvocationOwner>,
        session: Option<JsValue>,
    ) -> Result<ContextLease, RuntimeError> {
        if self.contexts.borrow().len() >= 128 {
            return Err(RuntimeError::unavailable(
                "contexts_limit",
                "browser context limit reached",
            ));
        }
        let token = fresh_context_token();
        self.contexts.borrow_mut().push(ContextEntry {
            token: token.clone(),
            context,
            invocation,
            session,
        });
        Ok(ContextLease {
            owner: self.clone(),
            token,
        })
    }

    pub(crate) fn report(&self, error: RuntimeError) {
        let mut errors = self.callback_errors.borrow_mut();
        if errors.len() == 128 {
            errors.remove(0);
        }
        errors.push(error);
    }
}

pub(crate) struct ContextLease {
    owner: Rc<WebHost>,
    pub(crate) token: JsValue,
}
impl Drop for ContextLease {
    fn drop(&mut self) {
        self.owner.remove_context(&self.token);
    }
}

/// An explicit browser owner of the platform-neutral Rust Host.
#[wasm_bindgen]
pub struct WasmHost {
    pub(crate) inner: Rc<WebHost>,
}

#[wasm_bindgen]
impl WasmHost {
    /// Creates a client Host with browser-local scheduling and no trusted ingress metadata.
    ///
    /// # Errors
    /// Refuses invalid text or an unavailable browser monotonic clock.
    #[wasm_bindgen(constructor)]
    pub fn new(name: JsValue) -> Result<WasmHost, JsValue> {
        Self::from_options(name, tembra_core::rpc::ConsumerLingerOptions::default())
    }

    /// Creates a Host with checked constructor-only consumer retention options.
    /// TTLs are whole milliseconds; omitted options retain the zero-linger default.
    ///
    /// # Errors
    /// Refuses invalid/unknown policy fields before creating a Driver or Host, in addition
    /// to the ordinary constructor's name and clock checks. No trusted identity is accepted.
    #[wasm_bindgen(js_name = withOptions)]
    pub fn with_options(name: JsValue, options: JsValue) -> Result<WasmHost, JsValue> {
        Self::from_options(name, crate::options::consumer_linger(&options)?)
    }
}

impl WasmHost {
    fn from_options(
        name: JsValue,
        consumer_linger: tembra_core::rpc::ConsumerLingerOptions,
    ) -> Result<Self, JsValue> {
        let name = text(&name)?;
        if name.is_empty() {
            return Err(failure(RuntimeError::invalid(
                "host_name",
                "host name is empty",
            )));
        }
        let driver = BrowserDriver::new().map_err(failure)?;
        Ok(Self {
            inner: Rc::new(WebHost {
                host: Host::new(
                    HostOptions::new(name, Value::Null).with_consumer_linger(consumer_linger),
                    driver,
                ),
                contexts: RefCell::default(),
                callback_errors: RefCell::default(),
            }),
        })
    }
}

#[wasm_bindgen]
impl WasmHost {
    /// Opens an outgoing client connection and installs its explicit provider route.
    ///
    /// # Errors
    /// Refuses invalid text, socket construction and core connection/route admission. This
    /// creates no trusted-host role; use the returned handle's ready Promise for the handshake.
    pub fn connect(&self, url: JsValue, provider: JsValue) -> Result<WasmConnection, JsValue> {
        let (url, provider) = (text(&url)?, text(&provider)?);
        let link = websocket_link(&url, BrowserLinkOptions::default()).map_err(failure)?;
        let connection = self
            .inner
            .host
            .connect_client(link, provider.clone(), ConnectionOptions::default())
            .map_err(failure)?;
        if let Err(error) = self.inner.host.set_route(provider, connection.clone()) {
            connection.close(CloseReason::new(error.clone()));
            return Err(failure(error));
        }
        Ok(WasmConnection { connection })
    }

    /// Acquires a Rust reference without waiting for shape or hydration.
    ///
    /// # Errors
    /// Refuses invalid descriptors/input/options and core admission failures. Browser options
    /// accept only state demand; a caller cannot assign trusted principal or execution roots.
    pub fn hold(
        &self,
        descriptor: JsValue,
        input: JsValue,
        options: JsValue,
    ) -> Result<WasmReference, JsValue> {
        let contract = Rc::new(
            tembra_contracts::Contract::from_json(&text(&descriptor)?).map_err(contract_failure)?,
        );
        let options = parse(&options)?;
        let options = options
            .as_object()
            .ok_or_else(|| failure(RuntimeError::invalid("options", "expected options object")))?;
        if options.keys().any(|key| key != "state") {
            return Err(failure(RuntimeError::invalid(
                "options",
                "unknown browser acquisition option",
            )));
        }
        let state = match options.get("state") {
            None => true,
            Some(Value::Bool(state)) => *state,
            Some(_) => {
                return Err(failure(RuntimeError::invalid(
                    "options",
                    "state must be boolean",
                )));
            }
        };
        let reference = self
            .inner
            .host
            .hold(
                contract,
                parse(&input)?,
                GetOptions {
                    state,
                    ..GetOptions::default()
                },
            )
            .map_err(failure)?;
        Ok(WasmReference {
            owner: self.inner.clone(),
            reference,
        })
    }

    /// Keeps a managed root alive until the callback's returned Promise settles.
    /// Rejection does not roll back callback effects; abandoning the Promise is not cancellation.
    #[wasm_bindgen(js_name = runRoot)]
    pub fn run_root(&self, callback: Function) -> Promise {
        let owner = self.inner.clone();
        future_to_promise(async move {
            let host = owner.host.clone();
            host.root(move |context| {
                async move {
                    let _lease = owner.issue_context(context)?;
                    let token = _lease.token.clone();
                    let result = callback
                        .call1(&JsValue::UNDEFINED, &token)
                        .map_err(|_| callback_error())?;
                    JsFuture::from(Promise::resolve(&result))
                        .await
                        .map_err(|_| callback_error())
                }
                .boxed_local()
            })
            .await
            .map_err(failure)
        })
    }

    /// Signals cancellation of an exact, live token issued by this Host.
    ///
    /// # Errors
    /// Refuses forged, foreign, settled or previously cancelled tokens without authority effects.
    #[wasm_bindgen(js_name = cancelContext)]
    pub fn cancel_context(&self, token: JsValue) -> Result<(), JsValue> {
        self.inner.context(&token)?.cancel("browser cancellation");
        Ok(())
    }

    /// Cancels issued roots and waits for managed cleanup. Arbitrary JS work remains cooperative.
    pub fn close(&self) -> Promise {
        let owner = self.inner.clone();
        future_to_promise(async move {
            let contexts = std::mem::take(&mut *owner.contexts.borrow_mut());
            for entry in contexts {
                entry.context.cancel("browser host closed");
            }
            owner.host.close().await.map_err(failure)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Returns bounded adapter callback failures plus the core's retained background failures.
    #[wasm_bindgen(js_name = errorsJson)]
    pub fn errors_json(&self) -> String {
        let mut errors = self.inner.host.errors();
        errors.extend(self.inner.callback_errors.borrow().iter().cloned());
        json!(errors).to_string()
    }
}

/// Logical connection status is separate from physical provider-handler cleanup.
#[wasm_bindgen]
pub struct WasmConnection {
    connection: Connection,
}

#[wasm_bindgen]
impl WasmConnection {
    pub fn status(&self) -> String {
        match self.connection.status() {
            ConnectionStatus::AwaitHello => "connecting",
            ConnectionStatus::Open => "open",
            ConnectionStatus::Closed => "closed",
        }
        .into()
    }

    pub fn ready(&self) -> Promise {
        let connection = self.connection.clone();
        future_to_promise(async move {
            connection.ready().await.map_err(failure)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    pub fn closed(&self) -> Promise {
        let connection = self.connection.clone();
        future_to_promise(async move {
            Ok(JsValue::from_str(
                &json!(connection.closed().await.error).to_string(),
            ))
        })
    }

    pub fn close(&self) {
        self.connection.close(CloseReason::closed());
    }
}

pub(crate) fn callback_error() -> RuntimeError {
    RuntimeError::unavailable("javascript_callback", "JavaScript callback failed")
}

pub(crate) fn status(status: RefStatus) -> &'static str {
    match status {
        RefStatus::Loading => "loading",
        RefStatus::Live => "live",
        RefStatus::Released => "released",
        RefStatus::Gone => "gone",
    }
}

pub(crate) fn shape(shape: Shape) -> Value {
    json!({"variant": shape.variant, "members": shape.members, "contract": shape.contract})
}

/// A reference capability whose implementation, dispatch and state remain in the Rust core.
#[wasm_bindgen]
pub struct WasmReference {
    pub(crate) owner: Rc<WebHost>,
    pub(crate) reference: Reference,
}

#[wasm_bindgen]
impl WasmReference {
    #[wasm_bindgen(js_name = statusJson)]
    pub fn status_json(&self) -> String {
        json!(status(self.reference.status())).to_string()
    }

    #[wasm_bindgen(js_name = shapeJson)]
    pub fn shape_json(&self) -> String {
        match self.reference.shape() {
            None => json!({"present": false}),
            Some(value) => json!({"present": true, "value": shape(value)}),
        }
        .to_string()
    }

    #[wasm_bindgen(js_name = errorJson)]
    pub fn error_json(&self) -> String {
        match self.reference.error() {
            None => json!({"present": false}),
            Some(value) => json!({"present": true, "value": value}),
        }
        .to_string()
    }

    pub fn ready(&self) -> Promise {
        let reference = self.reference.clone();
        future_to_promise(async move {
            reference.ready().await.map_err(failure)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Dispatches a portable call with explicit argument presence and an optional issued token.
    ///
    /// # Errors
    /// Refuses malformed payloads/text/tokens before dispatch. An error after admission can
    /// follow provider effects; disconnect is not evidence that replay would be safe.
    pub fn call(
        &self,
        member: JsValue,
        payload: JsValue,
        token: JsValue,
    ) -> Result<Promise, JsValue> {
        let member = text(&member)?;
        let payload = parse(&payload)?;
        let fields = payload.as_object().ok_or_else(|| {
            failure(RuntimeError::invalid(
                "payload",
                "expected presence envelope",
            ))
        })?;
        let args = match fields.get("present") {
            Some(Value::Bool(false)) if fields.len() == 1 => None,
            Some(Value::Bool(true)) if fields.len() == 2 && fields.contains_key("value") => {
                Some(fields["value"].clone())
            }
            _ => {
                return Err(failure(RuntimeError::invalid(
                    "payload",
                    "invalid presence envelope",
                )));
            }
        };
        let (context, physical) = if token.is_undefined() {
            (None, None)
        } else {
            let (context, physical) = self.owner.call_context(&token, None)?;
            (Some(context), physical)
        };
        let reference = self.reference.clone();
        Ok(future_to_promise(async move {
            let _physical = physical;
            let result = reference
                .call_json(&member, args, context.as_ref())
                .await
                .map_err(failure)?;
            Ok(JsValue::from_str(
                &match result {
                    None => json!({"present": false}),
                    Some(value) => json!({"present": true, "value": value}),
                }
                .to_string(),
            ))
        }))
    }

    pub fn release(&self) {
        self.reference.release();
    }

    /// Obtains a stable Rust readable without creating state demand from a cache peek.
    ///
    /// # Errors
    /// Refuses invalid text, unavailable members and ended references.
    pub fn readable(&self, member: JsValue) -> Result<WasmReadable, JsValue> {
        Ok(WasmReadable {
            readable: self.reference.readable(&text(&member)?).map_err(failure)?,
        })
    }

    #[wasm_bindgen(js_name = onStatus)]
    pub fn on_status(&self, callback: Function) -> WasmSubscription {
        let owner = Rc::downgrade(&self.owner);
        let stop = self.reference.on_status(move |value, error| {
            let value = json!({"status": status(value), "error": error}).to_string();
            if callback
                .call1(&JsValue::UNDEFINED, &JsValue::from_str(&value))
                .is_err()
                && let Some(owner) = owner.upgrade()
            {
                owner.report(callback_error());
            }
        });
        WasmSubscription::new(stop)
    }

    #[wasm_bindgen(js_name = onShape)]
    pub fn on_shape(&self, callback: Function) -> WasmSubscription {
        let owner = Rc::downgrade(&self.owner);
        let stop = self.reference.on_shape(move |value| {
            if callback
                .call1(
                    &JsValue::UNDEFINED,
                    &JsValue::from_str(&shape(value).to_string()),
                )
                .is_err()
                && let Some(owner) = owner.upgrade()
            {
                owner.report(callback_error());
            }
        });
        WasmSubscription::new(stop)
    }
}
