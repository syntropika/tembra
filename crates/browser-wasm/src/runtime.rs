use std::{cell::RefCell, rc::Rc};

use js_sys::Promise;
use serde_json::{Value, json};
use tembra_core::rpc::{CloseReason, Connection, ConnectionOptions, ConnectionStatus};
use tembra_core::{
    Context, Host, HostOptions, InvocationLease, InvocationOwner, RefStatus, RuntimeError, Shape,
};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

use crate::{
    BrowserDriver, BrowserLinkOptions,
    boundary::{failure, text},
    intrinsics::fresh_context_token,
    websocket_link,
};

pub(crate) struct WebHost {
    pub host: Host,
    pub driver: Rc<BrowserDriver>,
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
                    driver.clone(),
                ),
                driver,
                contexts: RefCell::default(),
                callback_errors: RefCell::default(),
            }),
        })
    }
}

#[wasm_bindgen]
impl WasmHost {
    /// Installs the embedding-issued Host incarnation for parent-child RPC.
    ///
    /// # Errors
    /// Refuses malformed identities or replacement of an existing incarnation.
    #[wasm_bindgen(js_name = rpcIdentity)]
    pub fn rpc_identity(&self, incarnation: JsValue) -> Result<(), JsValue> {
        self.inner
            .host
            .set_rpc_identity(
                tembra_core::rpc::RootOrigin::new(self.inner.host.name(), text(&incarnation)?)
                    .map_err(failure)?,
            )
            .map_err(failure)
    }
    /// Creates a bounded opaque-frame endpoint for a trusted child or parent.
    ///
    /// # Errors
    /// Refuses invalid peer names, missing identities and core connection/route admission.
    #[wasm_bindgen(js_name = rpcAttach)]
    pub fn rpc_attach(&self, peer: JsValue, child: bool) -> Result<WasmRpcEndpoint, JsValue> {
        let peer = text(&peer)?;
        let (endpoint, link) = crate::ipc::Endpoint::new();
        let connection = if child {
            self.inner.host.accept_child(
                link,
                peer.clone(),
                Value::Null,
                ConnectionOptions::default(),
            )
        } else {
            self.inner.host.connect_parent(
                link,
                peer.clone(),
                Value::Null,
                ConnectionOptions::default(),
            )
        }
        .map_err(failure)?;
        self.inner
            .host
            .set_route(peer, connection.clone())
            .map_err(failure)?;
        Ok(WasmRpcEndpoint {
            endpoint,
            connection,
        })
    }
    /// Opens an outgoing WebSocket client connection and sets its provider route.
    ///
    /// # Errors
    /// Refuses invalid text, socket creation and core connection/route admission.
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

/// Issued bounded frame endpoint for Worker drivers; payloads are decoded only by Rust core.
#[wasm_bindgen]
pub struct WasmRpcEndpoint {
    endpoint: Rc<crate::ipc::Endpoint>,
    connection: Connection,
}
#[wasm_bindgen]
impl WasmRpcEndpoint {
    /// Admits one frame to the bounded core Link input queue.
    ///
    /// # Errors
    /// Refuses non-text frames, closed endpoints and exhausted byte/frame budgets.
    pub fn receive(&self, frame: JsValue) -> Result<(), JsValue> {
        self.endpoint.receive(text(&frame)?).map_err(failure)
    }
    pub fn drain(&self) -> js_sys::Array {
        self.endpoint
            .drain()
            .into_iter()
            .map(JsValue::from)
            .collect()
    }
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
impl Drop for WasmRpcEndpoint {
    fn drop(&mut self) {
        self.connection.close(CloseReason::closed());
    }
}
