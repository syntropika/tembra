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
/// `owned_results` marks resource handles anywhere in the result; `stream_result` marks a
/// `stream<u8>` returned as the complete result of an asynchronous freestanding function.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceSignature<'a> {
    pub receiver: Option<&'a ResourceIdentity>,
    /// The result contains owned resources, possibly inside WIT containers.
    pub owned_results: bool,
    pub stream_result: bool,
}

impl PreparedContract {
    /// Encode the data part of a capability call. Implicit `self` travels separately as a
    /// host-issued loan; each owned result position names a payload-local attachment index.
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
        } else if signature.stream_result {
            // The result's data names its one payload attachment.
            self.validate(form, Ty::U32, &value, 0)?;
        } else {
            match function.result {
                Some(ty) => self.validate_capabilities(form, ty, &value)?,
                None => self.check_result(form, function, &value)?,
            }
            if let Some(result) = function.result {
                self.respell(result, &mut value)?;
            }
        }
        Ok(value)
    }

    /// Check exports for owned resources inside result containers, implicit method receiver
    /// borrows and whole byte-stream results of asynchronous freestanding functions.
    ///
    /// Futures, other stream elements, nested or argument streams, resource arguments and
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
    ///
    /// A checked reference view is a host-local reference capability, not a provider-owned
    /// resource: it needs no capability transport, and the role check already admitted its
    /// method data, so it passes as for the data profiles.
    pub fn check_import_resource_profile(&self, binding: &str) -> Result<(), Error> {
        if self.references().contains_key(binding) {
            return Ok(());
        }
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
                self.defined_resource(interface, *resource)?;
                signature.owned_results = true;
                if let FunctionKind::Constructor(declared) = function.kind
                    && self.resolved(Ty::Ref(declared)) != self.resolved(Ty::Ref(*resource))
                {
                    return Err(Error("constructor returns another resource type".into()));
                }
            } else {
                signature.owned_results = self.resource_result_type(interface, result, 0)?;
            }
        }
        if matches!(function.kind, FunctionKind::Constructor(_))
            && !function.result.is_some_and(|result| matches!(self.resolved(result), Ty::Ref(id) if matches!(self.kind(id), Kind::Own(_))))
        {
            return Err(Error(
                "resource constructor must return its owned resource".into(),
            ));
        }
        Ok(signature)
    }

    /// Validate the result profile recursively, while preserving nominal resource identity.
    fn resource_result_type(
        &self,
        interface: &Interface,
        ty: Ty,
        depth: usize,
    ) -> Result<bool, Error> {
        if depth > crate::model::MAX_DEPTH {
            return Err(Error("WIT value nesting exceeds its limit".into()));
        }
        let Ty::Ref(id) = ty else {
            self.supported(ty, depth)?;
            return Ok(false);
        };
        match self.kind(id) {
            Kind::Own(resource) => {
                self.defined_resource(interface, *resource)?;
                Ok(true)
            }
            Kind::Alias(inner) | Kind::List(inner) => {
                self.resource_result_type(interface, *inner, depth + 1)
            }
            Kind::Option(inner) => {
                if let Ty::Ref(id) = self.resolved(*inner)
                    && matches!(self.kind(id), Kind::Option(_))
                {
                    return Err(Error("nested options require a tagged option codec".into()));
                }
                self.resource_result_type(interface, *inner, depth + 1)
            }
            Kind::Record(fields) => fields.iter().try_fold(false, |found, field| {
                self.resource_result_type(interface, field.ty, depth + 1)
                    .map(|next| found || next)
            }),
            Kind::Tuple(types) => types.iter().try_fold(false, |found, ty| {
                self.resource_result_type(interface, *ty, depth + 1)
                    .map(|next| found || next)
            }),
            Kind::Variant(cases) => {
                cases
                    .iter()
                    .filter_map(|case| case.ty)
                    .try_fold(false, |found, ty| {
                        self.resource_result_type(interface, ty, depth + 1)
                            .map(|next| found || next)
                    })
            }
            Kind::Result { ok, err } => {
                [*ok, *err]
                    .into_iter()
                    .flatten()
                    .try_fold(false, |found, ty| {
                        self.resource_result_type(interface, ty, depth + 1)
                            .map(|next| found || next)
                    })
            }
            _ => self.supported(ty, depth).map(|()| false),
        }
    }

    /// Rewrite each owned-result index through its exact WIT resource identity.
    ///
    /// Shapes and all data leaves are checked before the first callback. The caller must redeem
    /// every attachment exactly once and reject unused attachments; numbers alone confer no
    /// authority. Callback failure may partially rewrite `value`, which must then be discarded.
    pub fn map_owned_result(
        &self,
        slot: u32,
        value: &mut Value,
        mut map: impl FnMut(&ResourceIdentity, u32) -> Result<u32, Error>,
    ) -> Result<(), Error> {
        self.resource_signature(slot)?;
        let function = self.function(self.slots()[slot as usize]);
        if let Some(ty) = function.result {
            self.validate_capabilities(Form::Native, ty, value)?;
            self.map_owned(ty, value, &mut map)?;
        } else {
            self.check_result(Form::Native, function, value)?;
        }
        Ok(())
    }

    fn map_owned(
        &self,
        ty: Ty,
        value: &mut Value,
        map: &mut impl FnMut(&ResourceIdentity, u32) -> Result<u32, Error>,
    ) -> Result<(), Error> {
        let Ty::Ref(id) = ty else {
            return Ok(());
        };
        match self.kind(id) {
            Kind::Own(resource) => {
                let identity = self
                    .resource_identity(Ty::Ref(*resource))
                    .ok_or_else(|| Error("invalid resource identity".into()))?;
                let index = value
                    .as_u64()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or_else(|| Error("resource index must be u32".into()))?;
                *value = Value::from(map(identity, index)?);
            }
            Kind::Alias(inner) => self.map_owned(*inner, value, map)?,
            Kind::Option(inner) if !value.is_null() => self.map_owned(*inner, value, map)?,
            Kind::List(inner) => {
                for value in value.as_array_mut().expect("checked list") {
                    self.map_owned(*inner, value, map)?;
                }
            }
            Kind::Record(fields) => {
                for (field, spelling) in fields.iter().zip(self.spellings(id)) {
                    self.map_owned(
                        field.ty,
                        value.get_mut(&**spelling).expect("checked field"),
                        map,
                    )?;
                }
            }
            Kind::Tuple(types) => {
                for (ty, value) in types
                    .iter()
                    .zip(value.as_array_mut().expect("checked tuple"))
                {
                    self.map_owned(*ty, value, map)?;
                }
            }
            Kind::Variant(cases) => {
                let ty = cases
                    .iter()
                    .find(|case| value["tag"] == case.name)
                    .expect("checked case")
                    .ty;
                if let Some(ty) = ty {
                    self.map_owned(ty, &mut value["val"], map)?;
                }
            }
            Kind::Result { ok, err } => {
                let ty = if value["tag"] == "ok" { *ok } else { *err };
                if let Some(ty) = ty {
                    self.map_owned(ty, &mut value["val"], map)?;
                }
            }
            _ => {}
        }
        Ok(())
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
