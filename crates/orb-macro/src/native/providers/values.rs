//! Native import types and codecs for the existing portable capability containers.
use super::super::{Kind, boundary_at, decode, encode, wit_kind};
use crate::{camel_ident, rust_ident, type_ident, wit::ConsumedTypes};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::Ident;
use wit_parser::{Handle, Type, TypeDefKind};

/// A WIT import's native types and portable conversions.
pub(super) struct Values<'a> {
    pub types: &'a ConsumedTypes,
    pub facade: &'a Ident,
}

impl Values<'_> {
    /// Check precisely the containers the portable boundary can carry, excluding explicit borrows.
    pub fn check(&self, ty: Type) -> ::core::result::Result<(), &'static str> {
        if wit_kind(ty, self.types, 1).is_ok() {
            return ::core::result::Result::Ok(());
        }
        let (_, kind) = self
            .types
            .definition(ty)
            .map_err(|_| "an unsupported primitive")?;
        match kind {
            TypeDefKind::Resource | TypeDefKind::Handle(Handle::Own(_)) => {
                ::core::result::Result::Ok(())
            }
            TypeDefKind::List(inner) => self.check(*inner),
            TypeDefKind::Option(inner) => {
                if matches!(
                    self.types.definition(*inner),
                    ::core::result::Result::Ok((_, TypeDefKind::Option(_)))
                ) {
                    return ::core::result::Result::Err(
                        "a nested option, which the boundary's JSON cannot carry",
                    );
                }
                self.check(*inner)
            }
            TypeDefKind::Tuple(tuple) => tuple.types.iter().try_for_each(|ty| self.check(*ty)),
            TypeDefKind::Record(record) => record.fields.iter().try_for_each(|f| self.check(f.ty)),
            TypeDefKind::Variant(variant) => variant
                .cases
                .iter()
                .filter_map(|c| c.ty)
                .try_for_each(|ty| self.check(ty)),
            TypeDefKind::Result(result) => [result.ok, result.err]
                .into_iter()
                .flatten()
                .try_for_each(|ty| self.check(ty)),
            TypeDefKind::Handle(Handle::Borrow(_)) => {
                ::core::result::Result::Err("an explicit resource borrow")
            }
            _ => ::core::result::Result::Err("a stream, future or unsupported WIT type"),
        }
    }

    /// Spell a native value, replacing WIT handles and their containing data types.
    pub fn ty(&self, ty: Type) -> TokenStream {
        if wit_kind(ty, self.types, 1).is_ok() {
            return self.types.owned(ty, 1);
        }
        let (id, kind) = self.types.definition(ty).expect("a checked type");
        match kind {
            TypeDefKind::Resource | TypeDefKind::Record(_) | TypeDefKind::Variant(_) => {
                self.types.native_path(id, 1)
            }
            TypeDefKind::Handle(Handle::Own(resource)) => self.ty(Type::Id(*resource)),
            TypeDefKind::List(inner) => {
                let inner = self.ty(*inner);
                quote! { ::std::vec::Vec<#inner> }
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
                let side =
                    |ty: ::core::option::Option<Type>| ty.map_or(quote! { () }, |ty| self.ty(ty));
                let (ok, err) = (side(result.ok), side(result.err));
                quote! { ::core::result::Result<#ok, #err> }
            }
            _ => unreachable!("a checked import type"),
        }
    }

    /// Declare native copies only for records and variants containing owned resources.
    pub fn declaration(&self, name: &str, ty: Type) -> ::core::option::Option<TokenStream> {
        if wit_kind(ty, self.types, 1).is_ok() || self.check(ty).is_err() {
            return ::core::option::Option::None;
        }
        let ident = type_ident(name);
        let (id, kind) = self.types.definition(ty).ok()?;
        let doc =
            format!(" The native imported WIT type `{name}`, retaining owned resource handles.");
        if matches!(
            kind,
            TypeDefKind::Resource | TypeDefKind::Record(_) | TypeDefKind::Variant(_)
        ) && matches!(ty, Type::Id(original) if original != self.types.native_alias(id))
        {
            let ty = self.ty(ty);
            return ::core::option::Option::Some(quote! { #[doc = #doc] pub type #ident = #ty; });
        }
        ::core::option::Option::Some(match kind {
            TypeDefKind::Record(record) => {
                let fields = record.fields.iter().map(|f| {
                    let name = rust_ident(&f.name);
                    let ty = self.ty(f.ty);
                    quote! { pub #name: #ty }
                });
                quote! { #[doc = #doc] pub struct #ident { #(#fields,)* } }
            }
            TypeDefKind::Variant(variant) => {
                let cases = variant.cases.iter().map(|c| {
                    let name = camel_ident(&c.name);
                    let payload = c.ty.map(|ty| {
                        let ty = self.ty(ty);
                        quote! { (#ty) }
                    });
                    quote! { #name #payload }
                });
                quote! { #[doc = #doc] pub enum #ident { #(#cases,)* } }
            }
            TypeDefKind::Resource => {
                if matches!(ty, Type::Id(original) if original == self.types.native_alias(id)) {
                    return ::core::option::Option::None;
                }
                let ty = self.ty(ty);
                quote! { #[doc = #doc] pub type #ident = #ty; }
            }
            _ => {
                let ty = self.ty(ty);
                quote! { #[doc = #doc] pub type #ident = #ty; }
            }
        })
    }

    /// Read a delivered value or move an argument into its existing portable payload.
    pub fn convert(&self, ty: Type, value: TokenStream, read: bool) -> TokenStream {
        let facade = self.facade;
        let boundary = boundary_at(1);
        if let ::core::result::Result::Ok(kind) = wit_kind(ty, self.types, 1) {
            return if read {
                decode(&kind, value, &boundary, facade)
            } else {
                encode(&kind, value, &boundary, facade)
            };
        }
        let (_, kind) = self.types.definition(ty).expect("a checked type");
        let mapped = |ty, value| self.convert(ty, value, read);
        let json = quote! { ::#facade::serde_json::Value };
        let error = quote! { ::std::string::String::from("invalid portable resource container") };
        let result = self.ty(ty);
        let body = match kind {
            TypeDefKind::Resource | TypeDefKind::Handle(Handle::Own(_)) => {
                if read {
                    let resource_ty = syn::parse2(result.clone()).expect("a type");
                    let kind = Kind::Resource {
                        ty: &resource_ty,
                        handle: result,
                    };
                    return decode(&kind, value, &boundary, facade);
                }
                quote! { __tembra_payload.attach_argument(value.into()).map(#json::from).map_err(|e| e.to_string()) }
            }
            TypeDefKind::List(inner) => {
                let item = mapped(*inner, quote! { item });
                if read {
                    quote! { match value { #json::Array(items) => items.into_iter().map(|item| #item).collect(), _ => ::core::result::Result::Err(#error) } }
                } else {
                    quote! { value.into_iter().map(|item| #item).collect::<::core::result::Result<::std::vec::Vec<_>, ::std::string::String>>().map(#json::Array) }
                }
            }
            TypeDefKind::Option(inner) => {
                let item = mapped(*inner, quote! { item });
                if read {
                    quote! { match value { #json::Null => ::core::result::Result::Ok(::core::option::Option::None), item => #item.map(::core::option::Option::Some) } }
                } else {
                    quote! { match value { ::core::option::Option::None => ::core::result::Result::Ok(#json::Null), ::core::option::Option::Some(item) => #item } }
                }
            }
            TypeDefKind::Tuple(tuple) => {
                let count = tuple.types.len();
                let names: ::std::vec::Vec<_> =
                    (0..count).map(|i| format_ident!("item_{i}")).collect();
                let items = tuple
                    .types
                    .iter()
                    .zip(&names)
                    .map(|(ty, n)| mapped(*ty, quote! { #n }));
                if read {
                    quote! { let items = value.as_array_mut().ok_or_else(|| #error)?; if items.len() != #count { return ::core::result::Result::Err(#error); } let mut items = ::core::mem::take(items).into_iter(); #(let #names = items.next().expect("checked tuple");)* ::core::result::Result::Ok((#(#items?,)*)) }
                } else {
                    quote! { let (#(#names,)*) = value; ::core::result::Result::Ok(#json::Array(::std::vec![#(#items?),*])) }
                }
            }
            TypeDefKind::Record(record) => {
                let names: ::std::vec::Vec<_> =
                    record.fields.iter().map(|f| rust_ident(&f.name)).collect();
                let keys: ::std::vec::Vec<_> = record
                    .fields
                    .iter()
                    .map(|f| tembra_contracts::javascript_name(&f.name))
                    .collect();
                if read {
                    let items = record.fields.iter().zip(&keys).map(|(f, key)| {
                        mapped(f.ty, quote! { object.remove(#key).ok_or_else(|| #error)? })
                    });
                    quote! { let mut object = match value { #json::Object(object) => object, _ => return ::core::result::Result::Err(#error) }; ::core::result::Result::Ok(#result { #(#names: #items?,)* }) }
                } else {
                    let items = record
                        .fields
                        .iter()
                        .zip(&names)
                        .map(|(f, name)| mapped(f.ty, quote! { #name }));
                    quote! { let #result { #(#names,)* } = value; let mut object = ::#facade::serde_json::Map::new(); #(object.insert(::std::string::String::from(#keys), #items?);)* ::core::result::Result::Ok(#json::Object(object)) }
                }
            }
            TypeDefKind::Variant(variant) => {
                let cases = variant
                    .cases
                    .iter()
                    .map(|c| {
                        (c.name.as_str(), c.ty, {
                            let name = camel_ident(&c.name);
                            quote! { #result::#name }
                        })
                    })
                    .collect::<::std::vec::Vec<_>>();
                self.tagged(&cases, read, false)
            }
            TypeDefKind::Result(result) => self.tagged(
                &[
                    ("ok", result.ok, quote! { ::core::result::Result::Ok }),
                    ("err", result.err, quote! { ::core::result::Result::Err }),
                ],
                read,
                true,
            ),
            _ => unreachable!("a checked import type"),
        };
        let input = if read {
            quote! { #json }
        } else {
            result.clone()
        };
        let output = if read { result } else { json };
        quote! { (|mut value: #input| -> ::core::result::Result<#output, ::std::string::String> { #body })(#value) }
    }

    /// Convert variant and result payloads without serializing their resource owners.
    fn tagged(
        &self,
        cases: &[(&str, ::core::option::Option<Type>, TokenStream)],
        read: bool,
        result: bool,
    ) -> TokenStream {
        let facade = self.facade;
        let arms = cases.iter().map(|(tag, ty, case)| {
            if read {
                let item = ty.map(|ty| self.convert(ty, quote! { object.remove("val").ok_or("missing payload")? }, true));
                let item = match item { ::core::option::Option::Some(item) => quote! { #case(#item?) }, ::core::option::Option::None if result => quote! { #case(()) }, ::core::option::Option::None => quote! { #case } };
                quote! { #tag => ::core::result::Result::Ok(#item) }
            } else {
                let (pattern, val) = match ty {
                    ::core::option::Option::Some(ty) => { let item = self.convert(*ty, quote! { item }, false); (quote! { #case(item) }, ::core::option::Option::Some(quote! { object.insert(::std::string::String::from("val"), #item?); })) }
                    ::core::option::Option::None if result => (quote! { #case(()) }, ::core::option::Option::None),
                    ::core::option::Option::None => (quote! { #case }, ::core::option::Option::None),
                };
                quote! { #pattern => { let mut object = ::#facade::serde_json::Map::new(); object.insert(::std::string::String::from("tag"), ::#facade::serde_json::Value::from(#tag)); #val ::core::result::Result::Ok(::#facade::serde_json::Value::Object(object)) } }
            }
        });
        if read {
            quote! { let mut object = match value { ::#facade::serde_json::Value::Object(object) => object, _ => return ::core::result::Result::Err(::std::string::String::from("expected a tagged value")) }; match object.get("tag").and_then(|v| v.as_str()).ok_or("missing tag")?.to_owned().as_str() { #(#arms,)* _ => ::core::result::Result::Err(::std::string::String::from("unknown tag")) } }
        } else {
            quote! { match value { #(#arms,)* } }
        }
    }
}
