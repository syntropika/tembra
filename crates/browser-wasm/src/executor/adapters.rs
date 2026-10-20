//! Synchronous legacy views of the actual selected JavaScript implementation.
use super::{
    Generation,
    factory::{BindingOwner, OutputBindings, read_output},
    handles::{WasmCapability, WasmState},
    support::*,
};
use crate::{boundary::failure, runtime::WebHost};
use js_sys::Array;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};
use tembra_core::{ContractAdapter, Result, contracts::Contract};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

pub(super) struct SelectedImplementation {
    pub(super) generation: Rc<Generation>,
    pub(super) owner: Rc<BindingOwner>,
    pub(super) projection: JsValue,
}

/// Exact synchronous adapter-output pass; this grants no object declarations or Context.
#[wasm_bindgen]
pub struct WasmImplementationView {
    active: Rc<Cell<bool>>,
    host: Weak<WebHost>,
    bindings: Rc<RefCell<OutputBindings>>,
}
impl WasmImplementationView {
    fn check(&self) -> Result<()> {
        if !self.active.get() {
            return Err(error("contract_adapter", "adapter output pass has ended"));
        }
        self.host
            .upgrade()
            .ok_or_else(|| error("foreign_host", "adapter Host has ended"))?;
        Ok(())
    }
}
#[wasm_bindgen]
impl WasmImplementationView {
    /// Adds an issued same-Host source to this private compatibility view without writing state.
    ///
    /// # Errors
    /// Refuses an ended pass, foreign source or duplicate member. The core checks legacy schemas.
    #[wasm_bindgen(js_name = exposeState)]
    pub fn expose_state(
        &self,
        name: JsValue,
        state: &WasmState,
    ) -> std::result::Result<(), JsValue> {
        self.check().map_err(failure)?;
        state.check_host(&self.host).map_err(failure)?;
        let name = string(&name).map_err(failure)?;
        let mut bindings = self.bindings.borrow_mut();
        if bindings.states.contains_key(&name) {
            return Err(failure(error(
                "duplicate",
                "duplicate adapted state exposure",
            )));
        }
        bindings.states.insert(name, state.source.clone());
        Ok(())
    }

    /// Adds an issued same-Host resource capability without opening or acquiring a resource.
    ///
    /// # Errors
    /// Refuses an ended pass, foreign capability or duplicate member. Nominal checks follow.
    #[wasm_bindgen(js_name = exposeOpaque)]
    pub fn expose_opaque(
        &self,
        name: JsValue,
        capability: &WasmCapability,
    ) -> std::result::Result<(), JsValue> {
        self.check().map_err(failure)?;
        if !Weak::ptr_eq(&self.host, &capability.host) {
            return Err(failure(error(
                "foreign_host",
                "capability belongs to another Host",
            )));
        }
        let name = string(&name).map_err(failure)?;
        let mut bindings = self.bindings.borrow_mut();
        if bindings.capabilities.contains_key(&name) {
            return Err(failure(error(
                "duplicate",
                "duplicate adapted opaque exposure",
            )));
        }
        bindings.capabilities.insert(
            name,
            (capability.nominal.clone(), capability.resource.clone()),
        );
        Ok(())
    }
}

struct EndPass(Rc<Cell<bool>>);
impl Drop for EndPass {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

pub(super) fn parse(
    generation: &Rc<Generation>,
    provider: &Contract,
    raw: &JsValue,
) -> Result<BTreeMap<u64, ContractAdapter>> {
    let mut adapters = BTreeMap::new();
    if raw.is_undefined() {
        return Ok(adapters);
    }
    if !Array::is_array(raw) {
        return Err(error("contract_adapter", "adapters must be an array"));
    }
    for entry in Array::from(raw).iter() {
        let keys = record_keys(&entry)?;
        if keys.length() != 3
            || keys.iter().any(|key| {
                !matches!(
                    key.as_string().as_deref(),
                    Some("provider" | "consumer" | "adapt")
                )
            })
        {
            return Err(error("contract_adapter", "invalid adapter metadata"));
        }
        let declared_provider = contract(&field(&entry, "provider")?)?;
        let consumer = contract(&field(&entry, "consumer")?)?;
        if declared_provider.fingerprint() != provider.fingerprint()
            || consumer.name() != provider.name()
            || consumer.kind() != provider.kind()
            || consumer.placement() != provider.placement()
            || consumer.version() == provider.version()
            || adapters.contains_key(&consumer.version())
        {
            return Err(error(
                "contract_adapter",
                "incompatible or duplicate adapter metadata",
            ));
        }
        let adapt = generation.callback(function(field(&entry, "adapt")?)?)?;
        let adapter_generation = generation.clone();
        let version = consumer.version();
        let adapter: ContractAdapter = Rc::new(move |source| {
            let selected = source
                .retained_owner::<SelectedImplementation>()
                .ok_or_else(|| {
                    error(
                        "foreign_implementation",
                        "adapter source has no JavaScript implementation",
                    )
                })?;
            if !Weak::ptr_eq(&selected.owner.host, &adapter_generation.session.host) {
                return Err(error(
                    "foreign_host",
                    "adapter source belongs to another Host",
                ));
            }
            if !Rc::ptr_eq(&selected.generation.session, &adapter_generation.session) {
                return Err(error(
                    "foreign_generation",
                    "adapter source belongs to another session",
                ));
            }
            let active = Rc::new(Cell::new(true));
            let bindings = Rc::new(RefCell::new(OutputBindings::default()));
            let pass = EndPass(active.clone());
            let args = Array::new();
            args.push(&selected.projection);
            args.push(
                &WasmImplementationView {
                    active,
                    host: selected.owner.host.clone(),
                    bindings: bindings.clone(),
                }
                .into(),
            );
            let output = adapt.sync(&args)?;
            drop(pass);
            let bindings = std::mem::take(&mut *bindings.borrow_mut());
            read_output(
                &adapter_generation,
                &consumer,
                selected.owner.clone(),
                output,
                Some(bindings),
                Some(source),
            )
        });
        adapters.insert(version, adapter);
    }
    Ok(adapters)
}
