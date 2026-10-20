use super::*;

pub(super) struct Parsed<'a> {
    pub method: &'a syn::TraitItemFn,
    pub member_name: String,
    pub docs: Vec<syn::Attribute>,
    pub state: bool,
    pub portable: bool,
    pub opaque: Option<LitStr>,
}

pub(super) fn parse(member: &TraitItem, remote: bool) -> syn::Result<Parsed<'_>> {
    let TraitItem::Fn(method) = member else {
        return Err(syn::Error::new_spanned(
            member,
            "only methods are supported in contracts",
        ));
    };
    let signature = &method.sig;
    if method.default.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
        || signature.constness.is_some()
        || signature.unsafety.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "contract members require concrete signatures without default bodies",
        ));
    }
    let Some(FnArg::Receiver(receiver)) = signature.inputs.first() else {
        return Err(syn::Error::new_spanned(
            signature,
            "contract members require &self",
        ));
    };
    if receiver.reference.is_none()
        || receiver.mutability.is_some()
        || receiver.colon_token.is_some()
    {
        return Err(syn::Error::new_spanned(
            receiver,
            "contract members require &self",
        ));
    }
    let ident = &signature.ident;
    let member_name = ident.unraw().to_string();
    if matches!(
        member_name.as_str(),
        "core" | "from_reference" | "with_context" | "clone"
    ) {
        return Err(syn::Error::new_spanned(
            ident,
            "member name is reserved by the typed reference API",
        ));
    }
    let mut docs: Vec<_> = method
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .cloned()
        .collect();
    let mut state = false;
    let mut portable = remote;
    let mut explicit_portable = false;
    let mut opaque = None;
    let mut seen_attributes = HashSet::new();
    for attribute in &method.attrs {
        let attribute_name = attribute
            .path()
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        if attribute_name != "doc" && !seen_attributes.insert(attribute_name) {
            return Err(syn::Error::new_spanned(
                attribute,
                "duplicate contract member attribute",
            ));
        }
        if attribute.path().is_ident("state") {
            if !matches!(attribute.meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "state does not accept arguments",
                ));
            }
            state = true;
        } else if attribute.path().is_ident("portable") {
            if !matches!(attribute.meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "portable does not accept arguments",
                ));
            }
            portable = true;
            explicit_portable = true;
        } else if attribute.path().is_ident("opaque") {
            if !matches!(attribute.meta, syn::Meta::List(_)) {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "opaque requires id = \"name@version\"",
                ));
            }
            attribute.parse_nested_meta(|meta| {
                if !meta.path.is_ident("id") {
                    return Err(meta.error("expected id"));
                }
                if opaque.is_some() {
                    return Err(meta.error("duplicate opaque id"));
                }
                opaque = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            })?;
            if opaque.as_ref().is_none_or(|id| id.value().is_empty()) {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "opaque requires a nonempty id",
                ));
            }
            if !opaque.as_ref().is_some_and(|id| {
                id.value().rsplit_once('@').is_some_and(|(name, version)| {
                    !name.is_empty()
                        && !name.contains('@')
                        && !name.chars().any(char::is_whitespace)
                        && !version.is_empty()
                        && version.bytes().all(|byte| byte.is_ascii_digit())
                })
            }) {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "opaque id must be name@version",
                ));
            }
        } else if !attribute.path().is_ident("doc") {
            return Err(syn::Error::new_spanned(
                attribute,
                "unsupported contract member attribute",
            ));
        }
    }
    if explicit_portable && (state || opaque.is_some()) {
        return Err(syn::Error::new_spanned(
            method,
            "state/opaque getters cannot use portable",
        ));
    }
    if docs.is_empty() {
        let summary = format!(
            "Contract member `{member_name}` in the managed provider and consumer interface."
        );
        docs.push(syn::parse_quote!(#[doc = #summary]));
    }
    Ok(Parsed {
        method,
        member_name,
        docs,
        state,
        portable,
        opaque,
    })
}
