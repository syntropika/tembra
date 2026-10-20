use syn::{
    Item, Path,
    visit_mut::{self, VisitMut},
};

use crate::{Result, error};

/// Resolve generated dependencies through the public facade without changing data literals.
pub(crate) fn rewrite(file: &mut syn::File) -> Result<()> {
    let mut facade = Facade;
    facade.visit_file_mut(file);
    annotate(&mut file.items)
}

struct Facade;

impl VisitMut for Facade {
    fn visit_path_mut(&mut self, path: &mut Path) {
        visit_mut::visit_path_mut(self, path);
        let Some(segment) = path.segments.first() else {
            return;
        };
        let replacement = match segment.ident.to_string().as_str() {
            "tembra_core" => "core",
            "tembra_contracts" => "contracts",
            "serde_json" => "serde_json",
            "serde" => "serde",
            _ => return,
        };
        let mut target: Path =
            syn::parse_str(&format!("::tembra::{replacement}")).expect("fixed Rust paths");
        target
            .segments
            .extend(path.segments.iter().skip(1).cloned());
        *path = target;
    }

    fn visit_attribute_mut(&mut self, attribute: &mut syn::Attribute) {
        if attribute.path().is_ident("derive") {
            if let Ok(mut paths) = attribute.parse_args_with(
                syn::punctuated::Punctuated::<Path, syn::Token![,]>::parse_terminated,
            ) {
                for path in &mut paths {
                    self.visit_path_mut(path);
                }
                *attribute = syn::parse_quote!(#[derive(#paths)]);
            }
        } else if !attribute.path().is_ident("serde") {
            visit_mut::visit_attribute_mut(self, attribute);
        }
    }
}

fn annotate(items: &mut [Item]) -> Result<()> {
    for item in items {
        let attributes = match item {
            Item::Struct(item) => Some(&mut item.attrs),
            Item::Enum(item) => Some(&mut item.attrs),
            Item::Mod(item) => {
                if let Some((_, items)) = &mut item.content {
                    annotate(items)?;
                }
                None
            }
            _ => None,
        };
        let Some(attributes) = attributes else {
            continue;
        };
        let mut serializable = false;
        for attribute in attributes
            .iter()
            .filter(|attribute| attribute.path().is_ident("derive"))
        {
            let paths = attribute
                .parse_args_with(
                    syn::punctuated::Punctuated::<Path, syn::Token![,]>::parse_terminated,
                )
                .map_err(|cause| error("generated_syntax", "derive", cause.to_string()))?;
            serializable |= paths.iter().any(|path| {
                path.segments.last().is_some_and(|segment| {
                    segment.ident == "Serialize" || segment.ident == "Deserialize"
                })
            });
        }
        if serializable {
            attributes.push(syn::parse_quote!(#[serde(crate = "::tembra::serde")]));
        }
    }
    Ok(())
}
