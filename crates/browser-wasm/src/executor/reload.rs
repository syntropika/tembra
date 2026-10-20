//! Administration over executable candidates in the existing browser Host.

use super::{Generation, WasmExecutor, WasmGeneration, context::WasmSignal, scope, support::*};
use crate::{boundary::failure, runtime::WebHost};
use futures::{
    FutureExt,
    future::{Either, LocalBoxFuture, select},
};
use js_sys::{Array, Function, Promise};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    time::Duration,
};
use tembra_core::{CancellationToken, ExecutableFacet, ReloadOptions, Result, RuntimeError};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

/// An executable declaration, retaining its real callbacks without installing services.
#[wasm_bindgen]
pub struct WasmFacetCandidate {
    host: Weak<WebHost>,
    facet: ExecutableFacet,
}

struct Pending {
    options: ReloadOptions,
    candidates: Vec<ExecutableFacet>,
    cancellation: CancellationToken,
    notification: Option<LocalBoxFuture<'static, RuntimeError>>,
}

/// Collects executable candidates before submitting one core-owned transaction.
#[wasm_bindgen]
pub struct WasmReloadBuilder {
    host: Weak<WebHost>,
    pending: RefCell<Option<Pending>>,
}

#[wasm_bindgen]
impl WasmReloadBuilder {
    /// Adds a candidate without running its setup or changing installed services.
    ///
    /// # Errors
    /// Refuses another Host or a consumed builder before copying candidate ownership.
    pub fn add(&self, candidate: &WasmFacetCandidate) -> std::result::Result<(), JsValue> {
        if !Weak::ptr_eq(&self.host, &candidate.host) {
            return Err(failure(error(
                "foreign_host",
                "candidate belongs to another Host",
            )));
        }
        let facet = candidate.facet.clone();
        let mut pending = self.pending.borrow_mut();
        let Some(pending) = pending.as_mut() else {
            drop(pending);
            return Err(failure(consumed()));
        };
        pending.candidates.push(facet);
        Ok(())
    }

    /// Attaches an issued readonly notification; it never grants epoch authority.
    ///
    /// # Errors
    /// Refuses a repeated signal or consumed builder. Cancellation requests apply only
    /// before the core's commit decision, without rolling back callback effects.
    pub fn signal(&self, signal: &WasmSignal) -> std::result::Result<(), JsValue> {
        let notification = signal.notification();
        let reason = if signal.aborted() {
            signal.notification().now_or_never()
        } else {
            None
        };
        let cancellation = {
            let mut state = self.pending.borrow_mut();
            let pending = state.as_mut().ok_or_else(|| failure(consumed()))?;
            if pending.notification.is_some() {
                return Err(failure(error(
                    "reload_signal",
                    "reload already has a signal",
                )));
            }
            pending.notification = Some(notification);
            pending.cancellation.clone()
        };
        if let Some(reason) = reason {
            cancellation.cancel(reason);
        }
        Ok(())
    }

    /// Submits the transaction immediately; the core owns it independently of this observer.
    ///
    /// # Errors
    /// Refuses a consumed builder or ended Host. The returned Promise preserves core
    /// preparation, gate, restoration and postdecision failure causes without claiming rollback.
    pub fn start(&self) -> std::result::Result<Promise, JsValue> {
        let pending = self
            .pending
            .borrow_mut()
            .take()
            .ok_or_else(|| failure(consumed()))?;
        let host = self.host.upgrade().ok_or_else(|| failure(closed()))?;
        let transaction = host.host.reload(pending.candidates, pending.options);
        Ok(future_to_promise(async move {
            let result = if let Some(notification) = pending.notification {
                match select(transaction, notification).await {
                    Either::Left((result, _)) => result,
                    Either::Right((reason, transaction)) => {
                        pending.cancellation.cancel(reason);
                        transaction.await
                    }
                }
            } else {
                transaction.await
            };
            let report = result.map_err(failure)?;
            Ok(JsValue::from_str(
                &json!({
                    "codeVersion": report.current_version(),
                    "previousVersion": report.previous_version(),
                    "changed": report.changed(),
                    "removed": report.removed(),
                    "preparedObjects": report.prepared_objects(),
                    "gatedObjects": report.gated_objects(),
                    "lingeringPhysical": report.lingering_physical(),
                })
                .to_string(),
            ))
        }))
    }
}

#[wasm_bindgen]
impl WasmExecutor {
    /// Prepares trusted, already evaluated code without running setup or installing it.
    ///
    /// # Errors
    /// Refuses generation budget, metadata, callback quota and Host failures. Closed artifact
    /// admission remains the loader's responsibility, before reserving this generation.
    pub fn prepare(
        &self,
        name: JsValue,
        uses: JsValue,
        setup: Function,
    ) -> std::result::Result<WasmFacetCandidate, JsValue> {
        self.reserve_generation(0.0)?.prepare(name, uses, setup)
    }

    /// Creates an administration builder; no transaction is admitted until start().
    ///
    /// # Errors
    /// Refuses malformed/unknown options or an ended Host without running candidate setup.
    #[wasm_bindgen(js_name = beginReload)]
    pub fn begin_reload(&self, flags: JsValue) -> std::result::Result<WasmReloadBuilder, JsValue> {
        self.host_id().map_err(failure)?;
        let mut options = options(strict_json(&flags).map_err(failure)?).map_err(failure)?;
        let cancellation = CancellationToken::new();
        options.cancellation = Some(cancellation.clone());
        Ok(WasmReloadBuilder {
            host: self.session.host.clone(),
            pending: RefCell::new(Some(Pending {
                options,
                candidates: Vec::new(),
                cancellation,
                notification: None,
            })),
        })
    }
}

#[wasm_bindgen]
impl WasmGeneration {
    /// Captures a budgeted executable candidate without invoking its setup callback.
    ///
    /// # Errors
    /// Refuses invalid metadata, callback quota or Host admission. Earlier loader evaluation
    /// effects and spent lifetime evaluation budget are not reversed.
    pub fn prepare(
        &self,
        name: JsValue,
        uses: JsValue,
        setup: Function,
    ) -> std::result::Result<WasmFacetCandidate, JsValue> {
        candidate(self.inner.clone(), name, uses, setup).map_err(failure)
    }
}

fn candidate(
    generation: Rc<Generation>,
    name: JsValue,
    uses: JsValue,
    setup: Function,
) -> Result<WasmFacetCandidate> {
    let host = generation.host()?;
    let name = string(&name)?;
    let values = strict_json(&uses)?;
    let uses = values
        .as_array()
        .ok_or_else(|| error("facet_uses", "expected descriptor array"))?
        .iter()
        .map(|value| contract(&JsValue::from_str(&value.to_string())))
        .collect::<Result<Vec<_>>>()?;
    let setup = generation.callback(setup)?;
    let expected = host.host.id();
    let facet = ExecutableFacet::foreign(name, uses, move |scope, declaration| {
        if scope.host_id() != expected {
            return Err(error("foreign_host", "facet belongs to another Host"));
        }
        let args = Array::new();
        args.push(&scope::WasmFacetScope::new(scope.clone(), &generation).into());
        args.push(&scope::WasmFacetRecorder::new(declaration.recorder(), &generation).into());
        if !setup.sync(&args)?.is_undefined() {
            return Err(error("facet_setup", "setup must return undefined"));
        }
        Ok(())
    })?;
    Ok(WasmFacetCandidate {
        host: Rc::downgrade(&host),
        facet,
    })
}

fn options(value: Value) -> Result<ReloadOptions> {
    let fields = value
        .as_object()
        .ok_or_else(|| error("reload_options", "expected options"))?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "remove" | "gateDeadline" | "retireDeadline" | "taskStopDeadline"
        )
    }) {
        return Err(error("reload_options", "unknown reload option"));
    }
    let mut options = ReloadOptions::default();
    if let Some(remove) = fields.get("remove") {
        options.remove = remove
            .as_array()
            .ok_or_else(|| error("reload_options", "remove must be an array"))?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| error("reload_options", "facet name must be text"))
            })
            .collect::<Result<Vec<_>>>()?;
    }
    for (key, target) in [
        ("gateDeadline", &mut options.gate_deadline),
        ("retireDeadline", &mut options.retire_deadline),
        ("taskStopDeadline", &mut options.task_stop_deadline),
    ] {
        if let Some(value) = fields.get(key) {
            let ms = value
                .as_f64()
                .filter(|ms| ms.is_finite() && *ms > 0.0 && *ms <= 9_007_199_254_740_991.0)
                .ok_or_else(|| {
                    error(
                        "reload_options",
                        "deadline must be positive finite milliseconds",
                    )
                })?;
            *target = Duration::from_secs_f64(ms / 1_000.0);
        }
    }
    Ok(options)
}
fn consumed() -> RuntimeError {
    error("reload_builder", "reload builder was consumed")
}
fn closed() -> RuntimeError {
    RuntimeError::unavailable("closed", "browser Host ended")
}
