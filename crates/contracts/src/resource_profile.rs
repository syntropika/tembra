//! Resource and byte-stream positions checked for the native capability transport.
//!
//! Profile checks describe shapes only. A handle index never grants authority: the executor
//! must redeem the corresponding typed attachment and check its issuer before lowering it.
//! A byte stream is admitted only as the whole result of an asynchronous freestanding function,
//! exported or imported; its end travels as a host-created attachment, never as data.
use crate::validate::Form;
use crate::{
    Error, Function, FunctionKind, Interface, Item, Kind, PreparedContract, ResourceIdentity, Ty,
};
use serde_json::Value;

/// Capability roles of a checked function. `receiver` is the implicit first `self` parameter;
/// `own_result` is an owned resource returned as the complete result; `stream_result` marks a
/// `stream<u8>` returned as the complete result of an asynchronous freestanding function.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceSignature<'a> {
    pub receiver: Option<&'a ResourceIdentity>,
    pub own_result: Option<&'a ResourceIdentity>,
    pub stream_result: bool,
}

impl PreparedContract {
    /// Encode the data part of a capability call. Implicit `self` travels separately as a
    /// host-issued loan; a top-level owned result names a payload-local attachment index.
    /// This validates shapes only. The caller must validate the attachment and its issuer.
    pub fn to_resource_wire(
        &self,
        slot: u32,
        arguments: bool,
        value: Value,
    ) -> Result<Value, Error> {
        self.resource_wire(Form::Native, slot, arguments, value)
    }

    /// Decode the data part of a capability call, preserving exact 64-bit data values.
    pub fn from_resource_wire(
        &self,
        slot: u32,
        arguments: bool,
        value: Value,
    ) -> Result<Value, Error> {
        self.resource_wire(Form::Wire, slot, arguments, value)
    }

    fn resource_wire(
        &self,
        form: Form,
        slot: u32,
        arguments: bool,
        mut value: Value,
    ) -> Result<Value, Error> {
        let signature = self.resource_signature(slot)?;
        let function = self.function(self.slots()[slot as usize]);
        if arguments {
            let parameters = &function.params[usize::from(signature.receiver.is_some())..];
            let values = value
                .as_array_mut()
                .ok_or_else(|| Error("WIT arguments must be an array".into()))?;
            if values.len() != parameters.len() {
                return Err(Error(format!(
                    "{}: expected {} data arguments",
                    function.name,
                    parameters.len()
                )));
            }
            for (parameter, value) in parameters.iter().zip(values) {
                self.validate(form, parameter.ty, value, 0)?;
                self.respell(parameter.ty, value)?;
            }
        } else if signature.own_result.is_some() || signature.stream_result {
            // The result's data names its one payload attachment.
            self.validate(form, Ty::U32, &value, 0)?;
        } else {
            self.check_result(form, function, &value)?;
            if let Some(result) = function.result {
                self.respell(result, &mut value)?;
            }
        }
        Ok(value)
    }

    /// Check exports for top-level owned results, implicit method receiver borrows and whole
    /// byte-stream results of asynchronous freestanding functions.
    ///
    /// Futures, other stream elements, nested or argument streams and handles, and
    /// foreign-defined result resources need additional transfer rules and are refused. The
    /// incoming HTTP handler remains outside this transport, served through its host-local WASI
    /// adapter.
    pub fn check_resource_profile(&self) -> Result<(), Error> {
        for entry in &self.model().exports {
            if crate::lower::is_ingress(entry) {
                continue;
            }
            if let Item::Interface(interface) = &entry.item {
                self.resource_interface(interface)?;
            }
        }
        Ok(())
    }

    /// Check one selected Orb import; host WASI resources use their own standard transport.
    pub fn check_import_resource_profile(&self, binding: &str) -> Result<(), Error> {
        let entry = self
            .model()
            .imports
            .iter()
            .find(|entry| entry.key == binding)
            .ok_or_else(|| Error(format!("unknown WIT import {binding}")))?;
        let Item::Interface(interface) = &entry.item else {
            return Err(Error("Orb import must be an interface".into()));
        };
        self.resource_interface(interface)
    }

    /// Inspect a bound function after validating the complete resource shape it uses.
    ///
    /// This does not open an execution path or admit a capability. Data-only transports must
    /// still use `check_value_profile` before installation.
    pub fn resource_signature(&self, slot: u32) -> Result<ResourceSignature<'_>, Error> {
        let slot = *self
            .slots()
            .get(slot as usize)
            .ok_or_else(|| Error("unknown WIT method slot".into()))?;
        let Item::Interface(interface) = &self.side(slot.exports)[slot.entry as usize].item else {
            unreachable!("slots address interface functions")
        };
        self.resource_function(interface, self.function(slot))
    }

    fn resource_interface(&self, interface: &Interface) -> Result<(), Error> {
        for resource in interface.resources.values() {
            let identity = self
                .resource_identity(Ty::Ref(*resource))
                .ok_or_else(|| Error("invalid resource binding".into()))?;
            if interface.contract.is_none()
                || identity.interface.is_none()
                || identity.name.is_none()
            {
                return Err(Error(
                    "an anonymous resource has no portable type identity".into(),
                ));
            }
        }
        for function in &interface.functions {
            self.resource_function(interface, function)?;
        }
        Ok(())
    }

    fn resource_function(
        &self,
        interface: &Interface,
        function: &Function,
    ) -> Result<ResourceSignature<'_>, Error> {
        let mut signature = ResourceSignature::default();
        match function.kind {
            FunctionKind::Method(resource) | FunctionKind::AsyncMethod(resource) => {
                signature.receiver = Some(self.defined_resource(interface, resource)?);
                let Some(first) = function.params.first() else {
                    return Err(Error(
                        "resource method requires its implicit self borrow".into(),
                    ));
                };
                let Ty::Ref(ty) = self.resolved(first.ty) else {
                    return Err(Error("resource method self must be a borrow".into()));
                };
                let Kind::Borrow(borrowed) = self.kind(ty) else {
                    return Err(Error("resource method self must be a borrow".into()));
                };
                if self.resolved(Ty::Ref(*borrowed)) != self.resolved(Ty::Ref(resource)) {
                    return Err(Error(
                        "resource method self refers to another resource".into(),
                    ));
                }
            }
            FunctionKind::Constructor(resource)
            | FunctionKind::Static(resource)
            | FunctionKind::AsyncStatic(resource) => {
                self.defined_resource(interface, resource)?;
            }
            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {}
            _ => {
                return Err(Error(
                    "resource getters and setters need an execution adapter".into(),
                ));
            }
        }
        for parameter in function
            .params
            .iter()
            .skip(usize::from(signature.receiver.is_some()))
        {
            self.supported(parameter.ty, 0)?;
        }
        if let Some(result) = function.result {
            if self.byte_stream(result) {
                if !matches!(function.kind, FunctionKind::AsyncFreestanding) {
                    return Err(Error(
                        "a byte stream is carried only as the whole result of an asynchronous \
                         freestanding function"
                            .into(),
                    ));
                }
                signature.stream_result = true;
            } else if let Ty::Ref(ty) = self.resolved(result)
                && let Kind::Own(resource) = self.kind(ty)
            {
                signature.own_result = Some(self.defined_resource(interface, *resource)?);
                if let FunctionKind::Constructor(declared) = function.kind
                    && self.resolved(Ty::Ref(declared)) != self.resolved(Ty::Ref(*resource))
                {
                    return Err(Error("constructor returns another resource type".into()));
                }
            } else {
                self.supported(result, 0)?;
            }
        }
        if matches!(function.kind, FunctionKind::Constructor(_)) && signature.own_result.is_none() {
            return Err(Error(
                "resource constructor must return its owned resource".into(),
            ));
        }
        Ok(signature)
    }

    /// Whether `ty` is exactly `stream<u8>`.
    pub(crate) fn byte_stream(&self, ty: Ty) -> bool {
        matches!(self.resolved(ty), Ty::Ref(id) if matches!(
            self.kind(id),
            Kind::Stream(Some(element)) if self.resolved(*element) == Ty::U8
        ))
    }

    /// Resource functions and results must use the defining interface, not an imported alias.
    fn defined_resource(
        &self,
        interface: &Interface,
        resource: u32,
    ) -> Result<&ResourceIdentity, Error> {
        let identity = self
            .resource_identity(Ty::Ref(resource))
            .ok_or_else(|| Error("function does not refer to a resource".into()))?;
        let (Some(owner), Some(name)) = (&identity.interface, &identity.name) else {
            return Err(Error(
                "an anonymous resource has no portable type identity".into(),
            ));
        };
        if interface.contract.as_ref() != Some(owner)
            || interface.resources.get(name).is_none_or(|declared| {
                self.resolved(Ty::Ref(*declared)) != self.resolved(Ty::Ref(resource))
            })
        {
            return Err(Error(
                "resource functions require their defining interface".into(),
            ));
        }
        Ok(identity)
    }
}
