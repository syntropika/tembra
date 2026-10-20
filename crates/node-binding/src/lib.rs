//! Node-API adapter. Every operation and callback runs on its creating JavaScript thread.
//! Pending Rust work is polled locally; no Host or Rc is transferred to a worker pool.
mod activity;
mod callbacks;
mod factory;
mod handles;
mod hooks;
mod ipc;
mod loader;
mod operations;
mod session;

use napi::{Env, bindgen_prelude::Function};
use napi_derive::napi;
use serde_json::{Value, json};
use session::Session;
use std::rc::Rc;

/// Private SDK transport. This class is intentionally absent from the public SDK declarations.
#[napi]
pub struct NodeBridge {
    inner: Rc<Session>,
}

#[napi]
impl NodeBridge {
    /// Creates one native core Host on the calling thread.
    /// # Errors
    /// Refuses scheduler, entropy, options and callback-reference allocation failures.
    #[napi(constructor)]
    pub fn new(
        env: Env,
        name: String,
        options: String,
        dispatch: Function<String, String>,
    ) -> napi::Result<Self> {
        Ok(Self {
            inner: Session::new(env, name, &options, dispatch)?,
        })
    }

    /// Executes a checked synchronous operation or returns an asynchronous observer identity.
    /// # Errors
    /// Refuses access from another thread or malformed transport JSON before dispatch.
    #[napi]
    pub fn request(
        &self,
        operation: String,
        handle: String,
        arguments: String,
    ) -> napi::Result<String> {
        self.inner.check_thread()?;
        let args: Value = serde_json::from_str(&arguments)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let _entered = self.inner.scheduler.runtime.enter();
        let _generation = self.inner.enter_handle(&handle);
        self.inner
            .handoffs
            .borrow_mut()
            .push(session::Handoffs::default());
        let result = operations::request(&self.inner, &operation, &handle, args);
        let adopted = self.inner.handoffs.borrow_mut().pop().unwrap_or_default();
        Ok(match result {
            Ok(value) => json!({"ok": value, "adopted": {"callbacks":adopted.callbacks,"values":adopted.values}}),
            Err(error) => json!({"error": error, "adopted": {"callbacks":adopted.callbacks,"values":adopted.values}}),
        }.to_string())
    }

    /// Advances ready local tasks without waiting for JavaScript Promise completion.
    /// # Errors
    /// Refuses cross-thread and recursive scheduler driving.
    #[napi]
    pub fn poll(&self) -> napi::Result<String> {
        self.inner.check_thread()?;
        self.inner
            .handoffs
            .borrow_mut()
            .push(session::Handoffs::default());
        let result = self.inner.scheduler.poll();
        let adopted = self.inner.handoffs.borrow_mut().pop().unwrap_or_default();
        result?;
        let events = std::mem::take(&mut *self.inner.events.borrow_mut());
        Ok(json!({"events":events,"adopted":{"callbacks":adopted.callbacks,"values":adopted.values}}).to_string())
    }

    /// Settles an already admitted JavaScript callback on its issuing Rust channel.
    /// # Errors
    /// Refuses foreign callback jobs and malformed settlement envelopes.
    #[napi]
    pub fn complete(&self, job: String, result: String) -> napi::Result<()> {
        self.inner.check_thread()?;
        let value: Value =
            serde_json::from_str(&result).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        self.inner
            .complete(&job, value)
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }
}

/// Identifies the environment-wide loader accounting authority without creating a Host.
/// # Errors
/// Refuses allocation of the environment cleanup hook or authority identity.
#[napi]
pub fn loader_authority(env: Env) -> napi::Result<String> {
    Ok(loader::Environment::get(env)?.authority.clone())
}
