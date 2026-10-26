//! One V8 realm executes callbacks for one host.

use super::bindings::*;
use super::*;

/// One V8 environment per host. Only prebundled JavaScript is accepted; dependencies are resolved
/// by SDK tooling before execution. This adapter provides neither Node nor DOM APIs.
pub struct Runtime {
    js: JsRuntime,
    native: Host,
    codec: Rc<dyn tembra::codec::Codec>,
}

impl Runtime {
    /// Create an embedded V8 host with the default portable codec.
    pub fn new(native: Host, sdk_source: impl Into<String>) -> Result<Self> {
        Self::with_codec(
            native,
            sdk_source,
            Rc::new(tembra::codec::Json),
            "Tembra.jsonCodec",
        )
    }

    /// Create one JS runtime using the supplied host-wide codec.
    pub fn with_codec(
        native: Host,
        sdk_source: impl Into<String>,
        codec: Rc<dyn tembra::codec::Codec>,
        js_codec: &str,
    ) -> Result<Self> {
        native.delegate_actions();
        let extensions = vec![
            tembra_ops::init(),
            super::bridge::tembra_callback_ops::init(),
        ];
        let mut js = JsRuntime::new(RuntimeOptions {
            extensions,
            ..Default::default()
        });
        js.op_state().borrow_mut().put(Rc::new(Bindings {
            callbacks: super::bridge::CallbackBridge::attach(&native),
            kernel: tembra_bridge::Kernel {
                host: native.kernel(),
            },
            native: native.clone(),
            codec: codec.clone(),
            timers: RefCell::new(BTreeMap::new()),
        }));
        js.execute_script("tembra:runtime", sdk_source.into())
            .map_err(js_error)?;
        js.execute_script(
            "tembra:host",
            format!("globalThis.host = Tembra.createHost({{ serialization: ({js_codec}) }});"),
        )
        .map_err(js_error)?;
        Ok(Self { js, native, codec })
    }

    /// Read the native host that shares this runtime's kernel.
    pub fn native(&self) -> &Host {
        &self.native
    }

    /// Calls an installed JS or Rust service through the shared managed host. Conversion occurs
    /// once at this language boundary; business logic and validation execute in the owning orb.
    pub async fn call(
        &mut self,
        service: &str,
        key: &[String],
        method: &str,
        arguments: Value,
    ) -> Result<Value> {
        let request = json!({
            "service": service,
            "key": key,
            "method": method,
            "arguments": arguments,
        });
        let bytes = self.codec.serialize(&request)?;
        {
            deno_core::scope!(scope, self.js);
            let length = bytes.len();
            let backing =
                v8::ArrayBuffer::new_backing_store_from_boxed_slice(bytes.into_boxed_slice())
                    .make_shared();
            let buffer = v8::ArrayBuffer::with_backing_store(scope, &backing);
            let bytes = v8::Uint8Array::new(scope, buffer, 0, length)
                .ok_or_else(|| Error::invalid(Reason::Memory, "cannot allocate boundary value"))?;
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__tembraRequest")
                .ok_or_else(|| Error::invalid(Reason::Memory, "cannot allocate boundary key"))?;
            global.set(scope, key.into(), bytes.into());
        }
        let value = self
            .resolve_script(concat!(
                "(async () => { const codec = host.session.codec; ",
                "const request = codec.deserialize(globalThis.__tembraRequest); ",
                "delete globalThis.__tembraRequest; ",
                "const reference = await host.get(request.service, request.key); ",
                "try { const result = await reference[request.method](...request.arguments); ",
                "return codec.serialize(result ?? null); ",
                "} finally { reference.release(); ",
                "} })()",
            ))
            .await?;
        let bytes = {
            deno_core::scope!(scope, self.js);
            let value = v8::Local::new(scope, value);
            let view = v8::Local::<v8::Uint8Array>::try_from(value).map_err(|_| {
                Error::invalid(Reason::ByteResult, "codec must return a Uint8Array")
            })?;
            let buffer = view
                .buffer(scope)
                .ok_or_else(|| Error::invalid(Reason::ByteResult, "codec result has no buffer"))?;
            if buffer.was_detached() {
                return Err(Error::invalid(
                    Reason::ByteResult,
                    "codec result buffer is detached",
                ));
            }
            let length = view.byte_length();
            // The safe V8 copying API takes an i32-sized destination, including shared views.
            if length > i32::MAX as usize {
                return Err(Error::invalid(
                    Reason::ByteResult,
                    "codec result is too large",
                ));
            }
            let mut bytes = vec![0; length];
            if view.copy_contents(&mut bytes) != length {
                return Err(Error::invalid(
                    Reason::ByteResult,
                    "codec result copy was incomplete",
                ));
            }
            bytes
        };
        self.codec.deserialize(&bytes)
    }

    /// Waits for a JavaScript result while driving V8's asynchronous operations.
    pub async fn evaluate(&mut self, source: impl Into<String>) -> Result<Value> {
        let value = self.resolve_script(source).await?;
        deno_core::scope!(scope, self.js);
        let local = v8::Local::new(scope, value);
        deno_core::serde_v8::from_v8(scope, local).map_err(js_error)
    }

    /// Drive the V8 event loop until the script promise settles.
    async fn resolve_script(&mut self, source: impl Into<String>) -> Result<v8::Global<v8::Value>> {
        let value = self
            .js
            .execute_script("tembra:application", source.into())
            .map_err(js_error)?;
        let resolved = self.js.resolve(value);
        self.js
            .with_event_loop_promise(resolved, Default::default())
            .await
            .map_err(js_error)
    }

    /// Install an embedded orb bundle through the shared JS adapter.
    pub async fn install(&mut self, name: &str, bundle: String) -> Result<()> {
        self.bundle(name, bundle, false).await
    }

    /// Replace orbs through the shared adapter's lifecycle entrypoint.
    pub async fn reload(&mut self, name: &str, bundle: String) -> Result<()> {
        self.bundle(name, bundle, true).await
    }

    /// Load one prebuilt JS bundle without runtime package resolution.
    async fn bundle(&mut self, _name: &str, bundle: String, reload: bool) -> Result<()> {
        // Executable bundles are scripts, so retired orbs are not pinned in V8's ESM module map.
        self.js
            .execute_script("tembra:orb", bundle)
            .map_err(js_error)?;
        let operation = if reload {
            "host.reload([globalThis.__tembraOrb])"
        } else {
            "host.install(globalThis.__tembraOrb)"
        };
        self.evaluate(format!(
            r#"
            (async () => {{
                try {{
                    await {operation};
                    return null;
                }} finally {{
                    delete globalThis.__tembraOrb;
                }}
            }})()
        "#
        ))
        .await?;
        Ok(())
    }

    /// Wait for JS cleanup and native physical callbacks to settle.
    pub async fn close(&mut self) -> Result<()> {
        let result = self.evaluate("host.close().then(() => null)").await;
        let native = self.native.finish_close();
        result?;
        native
    }
}

/// Convert an embedded runtime failure into a structured host error.
pub(super) fn js_error(error: impl std::fmt::Display) -> Error {
    Error::invalid(Reason::Javascript, error.to_string())
}
