use std::cell::RefCell;

use js_sys::Function;
use serde_json::{Value, json};
use tembra_core::{Readable, Snapshot, StateEvent};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

use crate::{boundary::failure, runtime::callback_error};

pub(crate) fn snapshot(snapshot: &Snapshot) -> Value {
    json!({"revision": snapshot.revision(), "value": snapshot.value()})
}

fn event(event: StateEvent) -> Value {
    let value = snapshot(event.snapshot());
    match event {
        StateEvent::Snapshot(_) => json!({"kind": "snapshot", "snapshot": value}),
        StateEvent::Reset(_) => json!({"kind": "reset", "snapshot": value}),
        StateEvent::Ops { delta, .. } => json!({"kind": "ops", "delta": delta, "snapshot": value}),
    }
}

/// A weak state capability; retaining this wrapper never adds an object claim.
#[wasm_bindgen]
pub struct WasmReadable {
    pub(crate) readable: Readable,
}

#[wasm_bindgen]
impl WasmReadable {
    /// Returns a detached JSON snapshot without requesting remote state demand.
    ///
    /// # Errors
    /// Refuses an unhydrated, ended or disallowed source. Cached method-only values may be stale.
    #[wasm_bindgen(js_name = snapshotJson)]
    pub fn snapshot_json(&self) -> Result<String, JsValue> {
        Ok(snapshot(&self.readable.snapshot().map_err(failure)?).to_string())
    }

    /// Waits for hydration with an explicit Rust interest, without extending an object claim.
    /// Promise abandonment is not cancellation; reference release or Host close terminates it.
    #[wasm_bindgen(js_name = awaitHydrated)]
    pub fn await_hydrated(&self) -> js_sys::Promise {
        let wait = self.readable.await_hydrated();
        future_to_promise(async move {
            Ok(JsValue::from_str(
                &snapshot(&wait.await.map_err(failure)?).to_string(),
            ))
        })
    }

    /// Delivers detached snapshots; explicit unsubscribe drops the core interest and callback.
    ///
    /// # Errors
    /// Refuses ended sources and subscription admission. Callback exceptions are observer
    /// failures after committed effects and cannot roll back state.
    pub fn subscribe(&self, callback: Function) -> Result<WasmSubscription, JsValue> {
        let subscription = self
            .readable
            .subscribe(move |value| {
                callback
                    .call1(
                        &JsValue::UNDEFINED,
                        &JsValue::from_str(&snapshot(&value).to_string()),
                    )
                    .map(|_| ())
                    .map_err(|_| callback_error())
            })
            .map_err(failure)?;
        Ok(WasmSubscription::new(move || subscription.unsubscribe()))
    }

    /// Delivers first snapshots, checked operation events and reset lineages from Rust.
    ///
    /// # Errors
    /// Has the admission and post-commit callback behavior of subscribe.
    #[wasm_bindgen(js_name = subscribeEvents)]
    pub fn subscribe_events(&self, callback: Function) -> Result<WasmSubscription, JsValue> {
        let subscription = self
            .readable
            .subscribe_events(move |value| {
                callback
                    .call1(
                        &JsValue::UNDEFINED,
                        &JsValue::from_str(&event(value).to_string()),
                    )
                    .map(|_| ())
                    .map_err(|_| callback_error())
            })
            .map_err(failure)?;
        Ok(WasmSubscription::new(move || subscription.unsubscribe()))
    }
}

/// Owns one callback registration. Unsubscribe is idempotent while alive; free the wrapper once.
#[wasm_bindgen]
pub struct WasmSubscription {
    cleanup: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl WasmSubscription {
    pub(crate) fn new(cleanup: impl FnOnce() + 'static) -> Self {
        Self {
            cleanup: RefCell::new(Some(Box::new(cleanup))),
        }
    }
}

#[wasm_bindgen]
impl WasmSubscription {
    pub fn unsubscribe(&self) {
        let cleanup = self.cleanup.borrow_mut().take();
        if let Some(cleanup) = cleanup {
            cleanup();
        }
    }
}

impl Drop for WasmSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}
