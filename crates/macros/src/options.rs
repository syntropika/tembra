use super::*;

#[derive(Default)]
pub(super) struct Options {
    pub remote: bool,
    pub name: Option<LitStr>,
    pub version: Option<LitInt>,
    pub input: Option<Type>,
    pub host: Option<LitStr>,
    pub variant: Option<LitStr>,
}

pub(super) fn parse(attributes: TokenStream) -> syn::Result<Options> {
    let mut options = Options::default();
    let mut seen_options = HashSet::new();
    syn::meta::parser(|meta| {
        let key = meta
            .path
            .get_ident()
            .map(ToString::to_string)
            .ok_or_else(|| meta.error("unsupported contract option"))?;
        let unique_key = if key == "local" || key == "remote" {
            "scope"
        } else {
            &key
        };
        if !seen_options.insert(unique_key.to_owned()) {
            return Err(meta.error("duplicate or conflicting contract option"));
        }
        if meta.path.is_ident("local") {
            options.remote = false;
        } else if meta.path.is_ident("remote") {
            options.remote = true;
        } else if meta.path.is_ident("name") {
            options.name = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("version") {
            options.version = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("input") {
            options.input = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("host") {
            options.host = Some(meta.value()?.parse()?);
        } else if meta.path.is_ident("variant") {
            options.variant = Some(meta.value()?.parse()?);
        } else {
            return Err(meta.error("unsupported contract option"));
        }
        Ok(())
    })
    .parse2(attributes)?;
    if !seen_options.contains("scope") {
        return Err(syn::Error::new(
            Span::call_site(),
            "contract requires exactly one explicit scope: local or remote",
        ));
    }
    Ok(options)
}
