//! Compare executable interfaces without trusting package names or arena-local type indexes.
use crate::{
    Error,
    codec::{Writer, write_function_kind},
    model::{Interface, Kind, MAX_DEPTH, PreparedContract, Ty},
};

/// Canonical structural signatures of one world side, keyed by world binding.
#[cfg(feature = "authoring")]
pub(crate) type Side = std::collections::BTreeMap<String, InterfaceShape>;

/// Check interface metadata that controls provider selection as well as callable signatures.
#[cfg(feature = "authoring")]
#[derive(PartialEq, Eq)]
pub(crate) struct InterfaceShape {
    contract: Option<String>,
    name: Option<String>,
    resources: Vec<u8>,
    functions: Vec<u8>,
}

impl PreparedContract {
    /// Identify one hook callback independently of its import/export spelling.
    pub fn function_signature(
        &self,
        binding: &str,
        method: &str,
        exports: bool,
    ) -> Result<String, Error> {
        use sha2::{Digest, Sha256};
        let slot = self.bind(binding, method, exports)?;
        let function = self.function(self.slots()[slot as usize]);
        let mut writer = Writer::default();
        writer.len(function.params.len());
        for parameter in &function.params {
            self.shape(&mut writer, parameter.ty, 0)?;
        }
        self.option_shape(&mut writer, function.result, 0)?;
        Ok(format!("{:x}", Sha256::digest(writer.finish())))
    }

    /// Compilers eliminate unused imports; compare every retained function against its declaration.
    #[cfg(feature = "authoring")]
    pub(crate) fn used_import_signatures(&self, actual: &Self) -> Result<Side, Error> {
        use crate::model::Item;
        let mut model = self.model().clone();
        model.imports.retain_mut(|entry| {
            let Some(retained) = actual
                .model()
                .imports
                .iter()
                .find(|other| other.key == entry.key)
            else {
                return false;
            };
            if let (Item::Interface(expected), Item::Interface(actual)) =
                (&mut entry.item, &retained.item)
            {
                // An unused resource declaration may disappear; actual resources must stay declared.
                expected
                    .resources
                    .retain(|name, _| actual.resources.contains_key(name));
                expected.functions.retain(|function| {
                    actual
                        .functions
                        .iter()
                        .any(|other| other.name == function.name)
                });
            }
            true
        });
        Self::new(model)?.signatures(false)
    }

    /// Describe complete signatures of one side, including ordered record fields and cases.
    #[cfg(feature = "authoring")]
    pub(crate) fn signatures(&self, exports: bool) -> Result<Side, Error> {
        use crate::model::{Entry, Item};
        let mut side = Side::new();
        let mut resources = std::collections::BTreeMap::new();
        let mut entries = self.side(exports).iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        for entry in entries {
            let Entry { key, item } = entry;
            match item {
                Item::Type(_) => {}
                Item::Function(_) => {
                    return Err(Error(
                        "compiled value exports require named interfaces".into(),
                    ));
                }
                Item::Interface(interface) => {
                    side.insert(
                        key.clone(),
                        InterfaceShape {
                            contract: interface.contract.clone(),
                            name: interface.name.clone(),
                            resources: self.resource_shape(interface, &mut resources)?,
                            // Verify actual component types independently of execution-profile
                            // admission, including nominal resources and their equality graph.
                            functions: self.component_shape(interface, &mut resources)?,
                        },
                    );
                }
            }
        }
        Ok(side)
    }

    /// Include resource-only interfaces and local type aliases in executable verification.
    #[cfg(feature = "authoring")]
    fn resource_shape(
        &self,
        interface: &Interface,
        resources: &mut std::collections::BTreeMap<u32, u32>,
    ) -> Result<Vec<u8>, Error> {
        let mut writer = Writer::default();
        writer.len(interface.resources.len());
        for (name, ty) in &interface.resources {
            writer.str(name);
            self.component_type(&mut writer, Ty::Ref(*ty), resources, 0)?;
        }
        Ok(writer.finish())
    }

    /// Compare host resource signatures without admitting those handles to the Orb value profile.
    #[cfg(feature = "authoring")]
    fn component_shape(
        &self,
        interface: &Interface,
        resources: &mut std::collections::BTreeMap<u32, u32>,
    ) -> Result<Vec<u8>, Error> {
        let mut functions = interface.functions.iter().collect::<Vec<_>>();
        functions.sort_by(|left, right| left.name.cmp(&right.name));
        let mut writer = Writer::default();
        writer.len(functions.len());
        for function in functions {
            writer.str(&function.name);
            writer.str(function.kind.as_str());
            if let Some(resource) = function.kind.resource() {
                self.component_type(&mut writer, Ty::Ref(resource), resources, 0)?;
            }
            writer.len(function.params.len());
            for param in &function.params {
                writer.str(&param.name);
                self.component_type(&mut writer, param.ty, resources, 0)?;
            }
            match function.result {
                Some(ty) => {
                    writer.byte(1);
                    self.component_type(&mut writer, ty, resources, 0)?;
                }
                None => writer.byte(0),
            }
        }
        Ok(writer.finish())
    }

    /// Canonicalize references by first use so resolver-local resource indices cannot affect equality.
    #[cfg(feature = "authoring")]
    fn component_type(
        &self,
        writer: &mut Writer,
        ty: Ty,
        resources: &mut std::collections::BTreeMap<u32, u32>,
        depth: usize,
    ) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error(
                "component signature nesting exceeds the contract limit".into(),
            ));
        }
        let Ty::Ref(id) = ty else {
            writer.ty(ty);
            return Ok(());
        };
        let kind = self.kind(id);
        if let Kind::Alias(ty) = kind {
            return self.component_type(writer, *ty, resources, depth + 1);
        }
        // Every compound kind has an explicit tag and an ordered sequence of immediate children.
        writer.byte(0x40);
        writer.str(kind.as_str());
        match kind {
            Kind::Resource(resource) => {
                let next = resources.len() as u32;
                writer.u32(*resources.entry(id).or_insert(next));
                // Preserve both nominal WIT identity and within-component type equality. A
                // forged owner/name must not relabel a binary's resource, while two distinct
                // component resources must not collapse merely because their names coincide.
                for name in [&resource.interface, &resource.name] {
                    writer.byte(u8::from(name.is_some()));
                    if let Some(name) = name {
                        writer.str(name);
                    }
                }
            }
            Kind::Record(fields) => {
                writer.len(fields.len());
                for field in fields {
                    writer.str(&field.name);
                }
            }
            Kind::Variant(cases) => {
                writer.len(cases.len());
                for case in cases {
                    writer.str(&case.name);
                    writer.byte(u8::from(case.ty.is_some()));
                }
            }
            Kind::Enum(names) | Kind::Flags(names) => {
                writer.len(names.len());
                for name in names {
                    writer.str(name);
                }
            }
            Kind::Result { ok, err } => {
                writer.byte(u8::from(ok.is_some()));
                writer.byte(u8::from(err.is_some()));
            }
            Kind::Future(ty) | Kind::Stream(ty) => writer.byte(u8::from(ty.is_some())),
            Kind::FixedList(_, length) => writer.u32(*length),
            _ => {}
        }
        let mut children = Vec::new();
        kind.children(|ty| children.push(ty));
        writer.len(children.len());
        for ty in children {
            self.component_type(writer, ty, resources, depth + 1)?;
        }
        Ok(())
    }

    /// Hash structural signatures for admission against the provider actually installed.
    ///
    /// The digest is always recomputed from the checked model, never read from an artifact.
    pub(crate) fn digest(&self, interface: &Interface) -> Result<String, Error> {
        use sha2::{Digest, Sha256};
        Ok(format!(
            "{:x}",
            Sha256::digest(self.interface_shape(interface)?)
        ))
    }

    /// Encode functions in name order with aliases expanded.
    fn interface_shape(&self, interface: &Interface) -> Result<Vec<u8>, Error> {
        let mut functions = interface.functions.iter().collect::<Vec<_>>();
        functions.sort_by(|left, right| left.name.cmp(&right.name));
        let mut writer = Writer::default();
        writer.len(functions.len());
        for function in functions {
            writer.str(&function.name);
            if let Some(resource) = function.kind.resource() {
                // A scoped function's arena index is local to the prepared contract. Admission
                // compares its declaring resource identity instead of encoding that index.
                writer.byte(0xff);
                writer.str(function.kind.as_str());
                self.shape(&mut writer, Ty::Ref(resource), 0)?;
            } else {
                write_function_kind(&mut writer, function.kind);
            }
            writer.len(function.params.len());
            for param in &function.params {
                writer.str(&param.name);
                self.shape(&mut writer, param.ty, 0)?;
            }
            self.option_shape(&mut writer, function.result, 0)?;
        }
        Ok(writer.finish())
    }

    fn option_shape(&self, writer: &mut Writer, ty: Option<Ty>, depth: usize) -> Result<(), Error> {
        match ty {
            Some(ty) => {
                writer.byte(1);
                self.shape(writer, ty, depth)
            }
            None => {
                writer.byte(0);
                Ok(())
            }
        }
    }

    /// Expand value aliases structurally and resource aliases by their declaring WIT identity.
    fn shape(&self, writer: &mut Writer, ty: Ty, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error(
                "component signature nesting exceeds the value profile".into(),
            ));
        }
        let Ty::Ref(id) = ty else {
            writer.ty(ty);
            return Ok(());
        };
        let depth = depth + 1;
        let names = |writer: &mut Writer, names: &[String]| {
            writer.len(names.len());
            names.iter().for_each(|name| writer.str(name));
        };
        match self.kind(id) {
            Kind::Alias(ty) => return self.shape(writer, *ty, depth),
            Kind::List(ty) => {
                writer.byte(0x40);
                self.shape(writer, *ty, depth)?;
            }
            Kind::Option(ty) => {
                writer.byte(0x41);
                self.shape(writer, *ty, depth)?;
            }
            Kind::Record(fields) => {
                writer.byte(0x42);
                writer.len(fields.len());
                for field in fields {
                    writer.str(&field.name);
                    self.shape(writer, field.ty, depth)?;
                }
            }
            Kind::Tuple(types) => {
                writer.byte(0x43);
                writer.len(types.len());
                for ty in types {
                    self.shape(writer, *ty, depth)?;
                }
            }
            Kind::Variant(cases) => {
                writer.byte(0x44);
                writer.len(cases.len());
                for case in cases {
                    writer.str(&case.name);
                    self.option_shape(writer, case.ty, depth)?;
                }
            }
            Kind::Result { ok, err } => {
                writer.byte(0x45);
                self.option_shape(writer, *ok, depth)?;
                self.option_shape(writer, *err, depth)?;
            }
            Kind::Flags(flags) => {
                writer.byte(0x46);
                names(writer, flags);
            }
            Kind::Enum(cases) => {
                writer.byte(0x47);
                names(writer, cases);
            }
            Kind::Resource(resource) => {
                let (Some(interface), Some(name)) = (&resource.interface, &resource.name) else {
                    return Err(Error(
                        "an anonymous resource has no portable type identity".into(),
                    ));
                };
                writer.byte(0x48);
                writer.str(interface);
                writer.str(name);
            }
            Kind::Own(resource) | Kind::Borrow(resource) => {
                writer.byte(if matches!(self.kind(id), Kind::Own(_)) {
                    0x49
                } else {
                    0x4a
                });
                self.shape(writer, Ty::Ref(*resource), depth)?;
            }
            // Digests describe the shape; which streams execute is the profile's decision.
            Kind::Stream(element) => {
                writer.byte(0x4b);
                self.option_shape(writer, *element, depth)?;
            }
            other => {
                return Err(Error(format!(
                    "{} requires a component value adapter",
                    other.as_str()
                )));
            }
        }
        Ok(())
    }
}
