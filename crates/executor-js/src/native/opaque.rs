use super::*;

pub(crate) trait ErasedOpaque {
    fn install(
        &self,
        candidate: Implementation,
        name: &str,
        resource: tembra_core::Resource<JsValue>,
    ) -> Result<Implementation>;
    fn read(&self, reference: &Reference, name: &str) -> Result<NativeValue>;
}
struct OpaqueAdapter<T> {
    codec: Rc<dyn ScopedCodec<Rc<T>>>,
}
impl<T: 'static> ErasedOpaque for OpaqueAdapter<T> {
    fn install(
        &self,
        candidate: Implementation,
        name: &str,
        resource: tembra_core::Resource<JsValue>,
    ) -> Result<Implementation> {
        let codec = self.codec.clone();
        Ok(candidate.map_opaque(name, resource, move |value| {
            let value = value.native.clone().ok_or_else(|| {
                invalid(
                    "native_capability",
                    "opaque resource must contain an explicitly issued native capability",
                )
            })?;
            codec.decode(NativeValue {
                inner: NativeData::Capability(value),
            })
        }))
    }
    fn read(&self, reference: &Reference, name: &str) -> Result<NativeValue> {
        let value = reference.opaque::<T>(name)?;
        let value = self.codec.encode(value)?;
        if !matches!(value.inner, NativeData::Capability(_)) {
            return Err(invalid(
                "native_capability",
                "opaque codec must issue an explicit capability",
            ));
        }
        Ok(value)
    }
}
impl Executor {
    /// Registers one nominal resource-to-opaque adapter without creating a synthetic resource.
    /// # Errors
    /// Refuses empty/control names, duplicate adapters or registry admission exhaustion.
    /// Core resource access still refuses factories, pending and closed resources.
    pub fn register_native_opaque<T: 'static>(
        &self,
        id: impl Into<String>,
        codec: Rc<dyn ScopedCodec<Rc<T>>>,
    ) -> Result<()> {
        let id = id.into();
        if id.is_empty() || id.chars().any(char::is_control) {
            return Err(invalid("native_signature", "invalid opaque nominal id"));
        }
        let mut adapters = self.inner.opaques.borrow_mut();
        if adapters.contains_key(&id) {
            return Err(invalid("native_signature", "duplicate opaque adapter"));
        }
        if adapters.len() + self.inner.natives.borrow().len() >= self.inner.options.max_callbacks {
            return Err(crate::engine::limit("native_adapters"));
        }
        adapters.insert(
            id,
            Rc::new(OpaqueAdapter {
                codec: safe_codec(codec),
            }),
        );
        Ok(())
    }
}
