use std::cell::RefCell;

use serde_json::{Value, json};
use tembra_core::Snapshot;
use wasm_bindgen::prelude::wasm_bindgen;

pub(crate) fn snapshot(snapshot: &Snapshot) -> Value {
    json!({"revision": snapshot.revision(), "value": snapshot.value()})
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
