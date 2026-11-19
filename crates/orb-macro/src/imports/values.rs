//! Owned import values and conversions to the private component bindings.
use crate::{camel_ident, rust_ident, type_ident, wit::ConsumedTypes};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use wit_parser::{Handle, Type, TypeDefKind, TypeId};

/// Type information shared by every operation of a component import.
pub(super) struct Values<'a> {
    pub types: &'a ConsumedTypes,
}

impl Values<'_> {
    /// Spell public owned parameters without exposing the raw resource handles.
    pub fn ty(&self, ty: Type) -> TokenStream {
        use Type::*;
        match ty {
            Bool => quote! { bool },
            U8 => quote! { u8 },
            U16 => quote! { u16 },
            U32 => quote! { u32 },
            U64 => quote! { u64 },
            S8 => quote! { i8 },
            S16 => quote! { i16 },
            S32 => quote! { i32 },
            S64 => quote! { i64 },
            F32 => quote! { f32 },
            F64 => quote! { f64 },
            Char => quote! { char },
            String => quote! { __tembra_alloc::string::String },
            Id(id) => {
                let (canonical, kind) = match self.types.definition(ty) {
                    Ok(found) => found,
                    Err(primitive) => return self.ty(primitive),
                };
                match kind {
                    TypeDefKind::Resource | TypeDefKind::Record(_) | TypeDefKind::Variant(_)
                        if self.types.carries_resource(ty) =>
                    {
                        self.types.native_path(canonical, 1)
                    }
                    TypeDefKind::Handle(Handle::Own(resource)) => self.ty(Id(*resource)),
                    TypeDefKind::List(inner) => {
                        let inner = self.ty(*inner);
                        quote! { __tembra_alloc::vec::Vec<#inner> }
                    }
                    TypeDefKind::Option(inner) => {
                        let inner = self.ty(*inner);
                        quote! { ::core::option::Option<#inner> }
                    }
                    TypeDefKind::Tuple(tuple) => {
                        let items = tuple.types.iter().map(|ty| self.ty(*ty));
                        quote! { (#(#items,)*) }
                    }
                    TypeDefKind::Result(result) => {
                        let side = |ty: Option<Type>| ty.map_or(quote! { () }, |ty| self.ty(ty));
                        let (ok, err) = (side(result.ok), side(result.err));
                        quote! { ::core::result::Result<#ok, #err> }
                    }
                    _ => self.types.imported_path(id, 1),
                }
            }
            ErrorContext => unreachable!("Orb imports do not carry error contexts"),
        }
    }

    /// Re-export data unchanged and replace only resource-bearing public types.
    pub fn declaration(&self, name: &str, id: TypeId) -> TokenStream {
        let ident = type_ident(name);
        let raw = self.types.imported_path(id, 1);
        if !self.types.carries_resource(Type::Id(id)) {
            return quote! { pub use #raw as #ident; };
        }
        let (canonical, kind) = self
            .types
            .definition(Type::Id(id))
            .expect("a resource type");
        if id != self.types.native_alias(canonical)
            && matches!(
                kind,
                TypeDefKind::Resource | TypeDefKind::Record(_) | TypeDefKind::Variant(_)
            )
        {
            let ty = self.ty(Type::Id(id));
            return quote! { pub type #ident = #ty; };
        }
        match kind {
            TypeDefKind::Resource => quote! {
                /// An owned WIT resource, released by its existing component handle.
                pub struct #ident { inner: #raw }
                impl ::core::convert::From<#raw> for #ident {
                    /// Preserve the original resource handle and nominal identity.
                    fn from(inner: #raw) -> Self { Self { inner } }
                }
                impl ::core::convert::From<#ident> for #raw {
                    /// Transfer the existing owner without duplicating it.
                    fn from(value: #ident) -> Self { value.inner }
                }
            },
            TypeDefKind::Record(record) => {
                let fields = record.fields.iter().map(|f| {
                    let name = rust_ident(&f.name);
                    let ty = self.ty(f.ty);
                    quote! { pub #name: #ty }
                });
                quote! { pub struct #ident { #(#fields,)* } }
            }
            TypeDefKind::Variant(variant) => {
                let cases = variant.cases.iter().map(|c| {
                    let name = camel_ident(&c.name);
                    let ty = c.ty.map(|ty| {
                        let ty = self.ty(ty);
                        quote! { (#ty) }
                    });
                    quote! { #name #ty }
                });
                quote! { pub enum #ident { #(#cases,)* } }
            }
            _ => {
                let ty = self.ty(Type::Id(id));
                quote! { pub type #ident = #ty; }
            }
        }
    }

    /// Rebuild resource containers while moving each original owner exactly once.
    pub fn convert(&self, ty: Type, value: TokenStream, read: bool) -> TokenStream {
        if !self.types.carries_resource(ty) {
            return value;
        }
        let (id, kind) = self.types.definition(ty).expect("a resource container");
        let mapped = |ty, value| self.convert(ty, value, read);
        match kind {
            TypeDefKind::Resource | TypeDefKind::Handle(Handle::Own(_)) => {
                quote! { ::core::convert::Into::into(#value) }
            }
            TypeDefKind::List(inner) => {
                let item = mapped(*inner, quote! { item });
                quote! { (#value).into_iter().map(|item| #item).collect() }
            }
            TypeDefKind::Option(inner) => {
                let item = mapped(*inner, quote! { item });
                quote! { (#value).map(|item| #item) }
            }
            TypeDefKind::Result(result) => {
                let ok = result
                    .ok
                    .map_or(quote! { item }, |ty| mapped(ty, quote! { item }));
                let err = result
                    .err
                    .map_or(quote! { item }, |ty| mapped(ty, quote! { item }));
                quote! { match #value { ::core::result::Result::Ok(item) => ::core::result::Result::Ok(#ok), ::core::result::Result::Err(item) => ::core::result::Result::Err(#err) } }
            }
            TypeDefKind::Tuple(tuple) => {
                let names: Vec<_> = (0..tuple.types.len())
                    .map(|i| format_ident!("item_{i}"))
                    .collect();
                let items = tuple
                    .types
                    .iter()
                    .zip(&names)
                    .map(|(ty, name)| mapped(*ty, quote! { #name }));
                quote! { { let (#(#names,)*) = #value; (#(#items,)*) } }
            }
            TypeDefKind::Record(record) => {
                let public = self.types.native_path(id, 1);
                let raw = self.types.imported_path(id, 1);
                let (from, to) = if read { (raw, public) } else { (public, raw) };
                let names: Vec<_> = record.fields.iter().map(|f| rust_ident(&f.name)).collect();
                let items = record
                    .fields
                    .iter()
                    .zip(&names)
                    .map(|(f, name)| mapped(f.ty, quote! { #name }));
                quote! { { let #from { #(#names,)* } = #value; #to { #(#names: #items,)* } } }
            }
            TypeDefKind::Variant(variant) => {
                let public = self.types.native_path(id, 1);
                let raw = self.types.imported_path(id, 1);
                let (from, to) = if read { (raw, public) } else { (public, raw) };
                let cases = variant.cases.iter().map(|case| {
                    let name = camel_ident(&case.name);
                    match case.ty {
                        Some(ty) => {
                            let item = mapped(ty, quote! { item });
                            quote! { #from::#name(item) => #to::#name(#item) }
                        }
                        None => quote! { #from::#name => #to::#name },
                    }
                });
                quote! { match #value { #(#cases,)* } }
            }
            _ => unreachable!("a supported owned resource container"),
        }
    }
}
