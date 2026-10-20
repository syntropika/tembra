use super::{
    Callback, Generation,
    context::WasmSignal,
    handles::*,
    native::{BrowserValue, NativeScope, has_native},
    support::*,
};
use crate::{boundary::failure, readable::snapshot, runtime::WebHost};
use futures::FutureExt;
use js_sys::{Array, Function};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};
use tembra_core::contracts::{Contract, Member};
use tembra_core::{
    DeclarationRecorder, GetOptions, Implementation, InvocationOwner, Readable,
    ResourceDeclaration, ResourceDependency, Result, StateDeclaration, WeakResource,
};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

pub(super) type Capabilities = BTreeMap<String, (String, WeakResource<BrowserValue>)>;

#[derive(Default)]
pub(super) struct OutputBindings {
    pub(super) states: BTreeMap<String, Readable>,
    pub(super) capabilities: Capabilities,
}
pub(crate) struct BindingOwner {
    pub(crate) activities: RefCell<super::activity::OwnerBindings>,
    pub(crate) recorder: DeclarationRecorder,
    pub(crate) invocation: InvocationOwner,
    pub(crate) generation: Weak<Generation>,
    pub(crate) host: Weak<WebHost>,
    states: RefCell<BTreeMap<String, Readable>>,
    capabilities: RefCell<Capabilities>,
}
impl BindingOwner {
    pub(crate) fn generation(&self) -> Result<Rc<Generation>> {
        generation(&self.generation)
    }
    pub(crate) fn recording(&self) -> Result<()> {
        self.recorder.invocation_owner().map(|_| ())
    }
}

/// Exact-pass declarations with independent weak state/resource handles after the pass.
#[wasm_bindgen]
pub struct WasmObjectRecorder {
    pub(crate) owner: Rc<BindingOwner>,
}
pub(crate) fn run_factory(
    generation: &Rc<Generation>,
    factory: &Rc<Callback>,
    contract: &Contract,
    input: Value,
    principal: Value,
    recorder: DeclarationRecorder,
) -> Result<Implementation> {
    let owner = Rc::new(BindingOwner {
        activities: RefCell::default(),
        invocation: recorder.invocation_owner()?,
        recorder,
        generation: Rc::downgrade(generation),
        host: generation.session.host.clone(),
        states: RefCell::default(),
        capabilities: RefCell::default(),
    });
    let args = Array::new();
    args.push(&JsValue::from_str(&input.to_string()));
    args.push(&JsValue::from_str(&principal.to_string()));
    args.push(
        &WasmObjectRecorder {
            owner: owner.clone(),
        }
        .into(),
    );
    let _pass = FactoryPass::enter(generation);
    let output = factory.sync(&args)?;
    read_output(generation, contract, owner, output, None, None)
}

pub(super) fn read_output(
    generation: &Rc<Generation>,
    contract: &Contract,
    owner: Rc<BindingOwner>,
    output: JsValue,
    bindings: Option<OutputBindings>,
    source: Option<Rc<Implementation>>,
) -> Result<Implementation> {
    let selected = field(&output, "variant")?;
    let variant = if selected.is_undefined() {
        None
    } else {
        Some(string(&selected)?)
    };
    let members = contract
        .members(variant.as_deref().unwrap_or_default())
        .ok_or_else(|| error("variant", "factory selected an unknown variant"))?;
    let methods = field(&output, "methods")?;
    let mut implementation = Implementation::new().retain_owner(owner.clone());
    if let Some(variant) = variant {
        implementation = implementation.variant(variant);
    }
    let aliases = field(&output, "aliases")?;
    if !aliases.is_undefined() {
        let source = source.as_ref().ok_or_else(|| {
            error(
                "contract_adapter",
                "method projection requires a selected implementation",
            )
        })?;
        for name in record_keys(&aliases)?.iter() {
            let name = string(&name)?;
            if !matches!(members.get(&name), Some(Member::Method { .. })) {
                return Err(error("member", "adapter alias is not a declared method"));
            }
            if !field(&methods, &name)?.is_undefined() {
                return Err(error(
                    "contract_adapter",
                    "method has both an alias and callback",
                ));
            }
            let target = string(&field(&aliases, &name)?)?;
            implementation = implementation.project_method(name, source.clone(), &target)?;
        }
    }
    for name in record_keys(&methods)?.iter() {
        let name = string(&name)?;
        let member = members
            .get(&name)
            .ok_or_else(|| error("member", "factory returned an undeclared method"))?;
        if !matches!(member, Member::Method { .. }) {
            return Err(error("member", "factory method has another member kind"));
        }
        let callback = generation.callback(function(field(&methods, &name)?)?)?;
        let owner = owner.clone();
        if has_native(member) {
            let scope = NativeScope {
                generation: generation.clone(),
                owner: owner.invocation.clone(),
            };
            let invoke = Rc::new(move |input: JsValue, context: tembra_core::Context| {
                let callback = callback.clone();
                let owner = owner.clone();
                async move {
                    let lease = owner.invocation.retain(&context)?;
                    let args = Array::new();
                    args.push(&input);
                    callback
                        .invoke(
                            Some(context),
                            args,
                            Some(lease),
                            Some(owner.invocation.clone()),
                        )
                        .await
                }
                .boxed_local()
            });
            implementation = generation.session.native.method(member)?.install(
                implementation,
                name,
                invoke,
                scope,
            );
        } else {
            implementation = implementation.json_method(name, move |input, context| {
                let callback = callback.clone();
                let owner = owner.clone();
                async move {
                    let lease = owner.invocation.retain(&context)?;
                    let args = Array::new();
                    args.push(&JsValue::from_str(&envelope(input).to_string()));
                    payload(strict_json(
                        &callback
                            .invoke(
                                Some(context),
                                args,
                                Some(lease),
                                Some(owner.invocation.clone()),
                            )
                            .await?,
                    )?)
                }
                .boxed_local()
            });
        }
    }
    let bindings = bindings.unwrap_or_else(|| OutputBindings {
        states: owner.states.borrow().clone(),
        capabilities: owner.capabilities.borrow().clone(),
    });
    for (name, state) in bindings.states {
        implementation = implementation.state(name, state);
    }
    for (name, (nominal, resource)) in bindings.capabilities {
        if !matches!(members.get(&name), Some(Member::Opaque { id }) if *id == nominal) {
            return Err(error(
                "opaque",
                "capability does not match the declared nominal member",
            ));
        }
        implementation = generation.session.native.opaque(&nominal)?.install(
            implementation,
            name,
            resource.upgrade()?,
        );
    }
    let projection = field(&output, "projection")?;
    if !projection.is_undefined() {
        record_keys(&projection)?;
        implementation =
            implementation.retain_owner(Rc::new(super::adapters::SelectedImplementation {
                generation: generation.clone(),
                owner,
                projection,
            }));
    }
    if let Some(source) = source {
        implementation = implementation.retain_owner(source);
    }
    Ok(implementation)
}
struct FactoryPass {
    generation: Rc<Generation>,
}
impl FactoryPass {
    fn enter(generation: &Rc<Generation>) -> Self {
        generation
            .session
            .factories
            .set(generation.session.factories.get() + 1);
        Self {
            generation: generation.clone(),
        }
    }
}
impl Drop for FactoryPass {
    fn drop(&mut self) {
        self.generation
            .session
            .factories
            .set(self.generation.session.factories.get() - 1);
    }
}

#[wasm_bindgen]
impl WasmObjectRecorder {
    /// # Errors
    /// Refuses expired/wrong-pass recording, invalid schema/version/value and duplicate
    /// declarations before initialization.
    pub fn cell(
        &self,
        name: JsValue,
        schema: JsValue,
        initial: JsValue,
        initialize: JsValue,
        cell_version: f64,
        migrate: Option<Function>,
    ) -> std::result::Result<WasmCell, JsValue> {
        self.owner.recording().map_err(failure)?;
        let generation = self.owner.generation().map_err(failure)?;
        let schema = strict_json(&schema).map_err(failure)?;
        let mut declaration = if initialize.is_undefined() {
            StateDeclaration::new(schema, strict_json(&initial).map_err(failure)?)
        } else {
            if !initial.is_undefined() {
                return Err(failure(error(
                    "state",
                    "cell has two initialization strategies",
                )));
            }
            let callback = generation
                .callback(function(initialize).map_err(failure)?)
                .map_err(failure)?;
            let owner = self.owner.clone();
            StateDeclaration::with_initializer(schema, move |context| {
                let callback = callback.clone();
                let _owner = owner.clone();
                async move {
                    let _owner = _owner;
                    strict_json(
                        &callback
                            .invoke(Some(context), Array::new(), None, None)
                            .await?,
                    )
                }
                .boxed_local()
            })
        }
        .version(version(cell_version).map_err(failure)?);
        if let Some(migrate) = migrate {
            let callback = generation.callback(migrate).map_err(failure)?;
            let owner = self.owner.clone();
            declaration = declaration.with_migrate(move |value, previous| {
                let _owner = &owner;
                let args = Array::new();
                args.push(&JsValue::from_str(&value.to_string()));
                args.push(&JsValue::from(previous as f64));
                strict_json(&callback.sync(&args)?)
            });
        }
        let cell = self
            .owner
            .recorder
            .state(string(&name).map_err(failure)?, declaration)
            .map_err(failure)?;
        Ok(WasmCell {
            cell,
            host: self.owner.host.clone(),
            generation: self.owner.generation.clone(),
        })
    }
    /// # Errors
    /// Refuses expired recording, another Host source or duplicate exposure. It does not mutate
    /// core state.
    #[wasm_bindgen(js_name = exposeState)]
    pub fn expose_state(
        &self,
        name: JsValue,
        state: &WasmState,
    ) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        state.check_host(&self.owner.host).map_err(failure)?;
        let name = string(&name).map_err(failure)?;
        if self.owner.states.borrow().contains_key(&name) {
            return Err(failure(error("duplicate", "duplicate state exposure")));
        }
        self.owner
            .states
            .borrow_mut()
            .insert(name, state.source.clone());
        Ok(())
    }
    /// # Errors
    /// Refuses expired recording, another Host capability or duplicate exposure; core later
    /// validates the selected nominal member.
    #[wasm_bindgen(js_name = exposeOpaque)]
    pub fn expose_opaque(
        &self,
        name: JsValue,
        capability: &WasmCapability,
    ) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        if !Weak::ptr_eq(&self.owner.host, &capability.host) {
            return Err(failure(error(
                "foreign_host",
                "capability belongs to another Host",
            )));
        }
        let name = string(&name).map_err(failure)?;
        if self.owner.capabilities.borrow().contains_key(&name) {
            return Err(failure(error("duplicate", "duplicate opaque exposure")));
        }
        self.owner.capabilities.borrow_mut().insert(
            name,
            (capability.nominal.clone(), capability.resource.clone()),
        );
        Ok(())
    }
    /// # Errors
    /// Refuses recorder, contract, input and dependency-cycle admission; this is an actual
    /// object-owned reference.
    pub fn get(
        &self,
        descriptor: JsValue,
        input: JsValue,
        state: bool,
    ) -> std::result::Result<WasmBoundReference, JsValue> {
        let reference = self
            .owner
            .recorder
            .get(
                contract(&descriptor).map_err(failure)?,
                strict_json(&input).map_err(failure)?,
                GetOptions {
                    state,
                    ..GetOptions::default()
                },
            )
            .map_err(failure)?;
        Ok(WasmBoundReference::new(
            reference,
            &self.owner.generation().map_err(failure)?,
        ))
    }
    /// # Errors
    /// Refuses expired recording or invalid flags/callbacks. No opener runs during recording.
    #[wasm_bindgen(js_name = beginResource)]
    pub fn begin_resource(
        &self,
        open: Function,
        close: JsValue,
        flags: JsValue,
    ) -> std::result::Result<WasmResourceBuilder, JsValue> {
        self.owner.recording().map_err(failure)?;
        let flags = strict_json(&flags).map_err(failure)?;
        let flags = flags
            .as_object()
            .ok_or_else(|| failure(error("resource", "expected resource flags")))?;
        if flags.keys().any(|k| k != "version" && k != "exclusive") {
            return Err(failure(error("resource", "unknown resource flag")));
        }
        let number = match flags.get("version") {
            None => 1.0,
            Some(value) => value
                .as_f64()
                .ok_or_else(|| failure(error("version", "resource version must be numeric")))?,
        };
        let version = version(number).map_err(failure)?;
        let exclusive = match flags.get("exclusive") {
            None => false,
            Some(Value::Bool(v)) => *v,
            _ => return Err(failure(error("resource", "exclusive must be boolean"))),
        };
        let close = if close.is_undefined() {
            None
        } else {
            Some(function(close).map_err(failure)?)
        };
        Ok(WasmResourceBuilder {
            owner: self.owner.clone(),
            open,
            close,
            version,
            exclusive,
            deps: RefCell::new(Some(Vec::new())),
        })
    }
    /// # Errors
    /// Refuses expired recording or invalid schema. The builder still requires checked actual
    /// sources.
    pub fn derive(
        &self,
        schema: JsValue,
        compute: Function,
    ) -> std::result::Result<WasmDerivationBuilder, JsValue> {
        self.owner.recording().map_err(failure)?;
        Ok(WasmDerivationBuilder {
            owner: self.owner.clone(),
            schema: strict_json(&schema).map_err(failure)?,
            compute,
            sources: RefCell::new(Some(Vec::new())),
        })
    }
    /// # Errors
    /// Returns recorder, registry, duplicate, metadata and host-lifetime failures without
    /// publishing a table.
    pub fn register(
        &self,
        registry: JsValue,
        key: JsValue,
        data: JsValue,
        member: JsValue,
    ) -> std::result::Result<(), JsValue> {
        let registry = super::scope::registry(&registry).map_err(failure)?;
        let key = string(&key).map_err(failure)?;
        let data = strict_json(&data).map_err(failure)?;
        if member.is_undefined() {
            self.owner.recorder.register(&registry, key, data)
        } else {
            self.owner.recorder.register_method(
                &registry,
                key,
                data,
                string(&member).map_err(failure)?,
            )
        }
        .map_err(failure)
    }
}

/// Resource builder holds no strong resource record and forwards its final declaration once.
#[wasm_bindgen]
pub struct WasmResourceBuilder {
    owner: Rc<BindingOwner>,
    open: Function,
    close: Option<Function>,
    version: u64,
    exclusive: bool,
    deps: RefCell<Option<Vec<ResourceDependency>>>,
}
#[wasm_bindgen]
impl WasmResourceBuilder {
    /// # Errors
    /// Refuses expired recording, another Host or a consumed builder. Core checks actual object
    /// ownership on finish.
    #[wasm_bindgen(js_name = referenceDependency)]
    pub fn reference_dependency(
        &self,
        reference: &WasmBoundReference,
    ) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        if !Weak::ptr_eq(&self.owner.host, &reference.host) {
            return Err(failure(error(
                "foreign_host",
                "reference belongs to another Host",
            )));
        }
        self.deps
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| failure(error("declaration_scope", "resource builder consumed")))?
            .push(ResourceDependency::Reference(reference.reference.clone()));
        Ok(())
    }
    /// # Errors
    /// Refuses expired recording, another Host, an ended resource or a consumed builder.
    #[wasm_bindgen(js_name = resourceDependency)]
    pub fn resource_dependency(&self, resource: &WasmResource) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        if !Weak::ptr_eq(&self.owner.host, &resource.host) {
            return Err(failure(error(
                "foreign_host",
                "resource belongs to another Host",
            )));
        }
        let dependency = resource.resource.upgrade().map_err(failure)?.dependency();
        self.deps
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| failure(error("declaration_scope", "resource builder consumed")))?
            .push(dependency);
        Ok(())
    }
    /// # Errors
    /// Refuses expired recording, consumed builder or core declaration errors. A final attempt
    /// consumes its candidate inputs; external effects are not rolled back.
    pub fn finish(&self, name: JsValue) -> std::result::Result<WasmResource, JsValue> {
        self.owner.recording().map_err(failure)?;
        let generation = self.owner.generation().map_err(failure)?;
        let open = generation.callback(self.open.clone()).map_err(failure)?;
        let owner = self.owner.clone();
        let mut declaration = ResourceDeclaration::new(move |signal| {
            let callback = open.clone();
            let _owner = owner.clone();
            async move {
                let _owner = _owner;
                let args = Array::new();
                args.push(&WasmSignal::token(signal).into());
                Ok(BrowserValue(callback.invoke(None, args, None, None).await?))
            }
            .boxed_local()
        });
        if let Some(close) = self.close.as_ref() {
            let close = generation.callback(close.clone()).map_err(failure)?;
            let owner = self.owner.clone();
            declaration = declaration.with_close(move |value| {
                let callback = close.clone();
                let _owner = owner.clone();
                async move {
                    let _owner = _owner;
                    let args = Array::new();
                    args.push(&value.0);
                    callback.invoke(None, args, None, None).await?;
                    Ok(())
                }
                .boxed_local()
            });
        }
        declaration.version = self.version;
        declaration.exclusive = self.exclusive;
        let deps = self
            .deps
            .borrow_mut()
            .take()
            .ok_or_else(|| failure(error("declaration_scope", "resource builder consumed")))?;
        declaration.deps = deps;
        let resource = self
            .owner
            .recorder
            .resource(string(&name).map_err(failure)?, declaration)
            .map_err(failure)?;
        Ok(WasmResource {
            resource: resource.downgrade(),
            host: self.owner.host.clone(),
            generation: self.owner.generation.clone(),
        })
    }
}

/// Actual synchronous core projection, with explicit checked source handles.
#[wasm_bindgen]
pub struct WasmDerivationBuilder {
    owner: Rc<BindingOwner>,
    schema: Value,
    compute: Function,
    sources: RefCell<Option<Vec<Readable>>>,
}
#[wasm_bindgen]
impl WasmDerivationBuilder {
    /// # Errors
    /// Refuses expired recording, another Host or a consumed builder; no projection runs here.
    pub fn source(&self, state: &WasmState) -> std::result::Result<(), JsValue> {
        self.owner.recording().map_err(failure)?;
        state.check_host(&self.owner.host).map_err(failure)?;
        self.sources
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| failure(error("declaration_scope", "derive builder consumed")))?
            .push(state.source.clone());
        Ok(())
    }
    /// # Errors
    /// Refuses expired recording, consumed builder or core declaration errors. A final attempt
    /// consumes its candidate inputs; external effects are not rolled back.
    pub fn finish(&self) -> std::result::Result<WasmState, JsValue> {
        self.owner.recording().map_err(failure)?;
        let generation = self.owner.generation().map_err(failure)?;
        let callback = generation.callback(self.compute.clone()).map_err(failure)?;
        let owner = self.owner.clone();
        let sources = self
            .sources
            .borrow_mut()
            .take()
            .ok_or_else(|| failure(error("declaration_scope", "derive builder consumed")))?;
        let source = self
            .owner
            .recorder
            .derive(sources, self.schema.clone(), move |values| {
                let _owner = &owner;
                let args = Array::new();
                args.push(&JsValue::from_str(
                    &Value::Array(values.iter().map(snapshot).collect()).to_string(),
                ));
                strict_json(&callback.sync(&args)?)
            })
            .map_err(failure)?;
        Ok(WasmState::new(source, &generation))
    }
}
