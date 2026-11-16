//! `#[cfg(..)]` on a service, its impl blocks, its methods and its create, on the root, on a hook
//! and on a sub-module holding one of these: the module comes back as one copy per combination of
//! their predicates, each under its own `#[cfg]` and the macro's attribute, which the macro expands
//! as any module.
use super::{expand, expansion, metadata::Metadata, options};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Expr, ImplItem, Item, ItemMod, Lit};

/// The macro's options in these tests.
fn attribute() -> TokenStream {
    quote!(package = "example:notes@1.0.0")
}

/// The copies the expansion of `module` writes under the default options.
fn copies(module: &str) -> Vec<(String, ItemMod)> {
    copies_with(attribute(), module)
}

/// The copies the expansion of `module` under `attribute` writes: each one's predicate, from the
/// `#[cfg]` before it, and the module, once checked that the macro's attribute follows with the
/// same options, and then the attribute naming the copy's combination, which the module keeps, as
/// the expansion of the copy reads it.
fn copies_with(attribute: TokenStream, module: &str) -> Vec<(String, ItemMod)> {
    let output = expansion(attribute.clone(), syn::parse_str(module).unwrap());
    let file: syn::File =
        syn::parse2(output.clone()).unwrap_or_else(|error| panic!("{error}\n{output}"));
    file.items
        .into_iter()
        .map(|item| {
            let Item::Mod(mut copy) = item else {
                panic!("a copy of the module\n{output}");
            };
            let guard = copy.attrs.remove(0);
            let orb = copy.attrs.remove(0);
            assert_eq!(
                quote!(#orb).to_string(),
                quote!(#[::tembra::orb(#attribute)]).to_string()
            );
            let syn::Meta::List(guard) = guard.meta else {
                panic!("a #[cfg(..)] before the copy\n{output}");
            };
            assert!(guard.path.is_ident("cfg"), "{output}");
            let guard = guard.tokens.to_string();
            let marker = &copy.attrs[0];
            assert_eq!(
                quote!(#marker).to_string(),
                format!("# [__tembra_configuration ({guard})]")
            );
            (guard, copy)
        })
        .collect()
}

/// The module the macro writes for a copy, as the expansion of the copy would, without the
/// manifest table of a crate.
fn expanded(copy: &ItemMod) -> ItemMod {
    expanded_with(attribute(), copy)
}

/// The module the macro writes for a copy under `attribute`.
fn expanded_with(attribute: TokenStream, copy: &ItemMod) -> ItemMod {
    let options = options(attribute).unwrap();
    let tokens = expand(options, copy.clone(), Metadata::named("calculator"), None)
        .unwrap_or_else(|error| panic!("{error}"));
    syn::parse2(tokens).unwrap()
}

/// The text of a string constant of a module the macro wrote.
fn constant(module: &ItemMod, name: &str) -> Option<String> {
    module
        .content
        .as_ref()?
        .1
        .iter()
        .find_map(|item| match item {
            Item::Const(constant) if constant.ident == name => match &*constant.expr {
                Expr::Lit(literal) => match &literal.lit {
                    Lit::Str(text) => Some(text.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
}

/// The functions of the impl blocks of a copy, in order.
fn functions(copy: &ItemMod) -> Vec<String> {
    let items = &copy.content.as_ref().expect("a body").1;
    items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(block) => Some(block.items.iter().filter_map(|member| match member {
                ImplItem::Fn(function) => Some(function.sig.ident.to_string()),
                _ => None,
            })),
            _ => None,
        })
        .flatten()
        .collect()
}

const NOTEBOOK: &str = r#"mod notebook {
    #[inputs]
    pub struct Inputs {
        pub label: String,
        #[cfg(feature = "archive")]
        pub shelf: String,
    }

    #[service]
    pub struct Notes;

    impl Notes {
        pub fn add(text: String) -> u32 {
            text.len() as u32
        }

        #[cfg(feature = "archive")]
        pub fn archive() -> u32 {
            0
        }

        #[cfg(test)]
        fn helper() {}
    }

    #[cfg(feature = "archive")]
    #[service]
    pub struct Shelf;

    #[cfg(feature = "archive")]
    impl Shelf {
        pub fn count() -> u32 {
            0
        }
    }
}"#;

/// A feature that adds a method and a service: the module comes back as two copies, one where
/// the predicate holds and one where it does not, each without the `#[cfg]`s the macro evaluated
/// and without the items its combination rules out. Each copy has its own WIT, the signature of
/// its interfaces, its component bridges and its own `orb()`, and expands as any module, with no
/// copies of its own and without the attribute naming its combination. A `#[cfg]` the macro does
/// not read stays for the compiler: on a field of the `#[inputs]` struct, which `orb(..)` takes
/// whole, or on a function that is no method.
#[test]
fn a_cfg_on_a_service_or_a_method_expands_one_copy_per_combination() {
    let copies = copies(NOTEBOOK);
    let guards: Vec<&str> = copies.iter().map(|(guard, _)| guard.as_str()).collect();
    assert_eq!(
        guards,
        [
            "all (not (feature = \"archive\"))",
            "all (feature = \"archive\")"
        ]
    );
    let Ok([(_, without), (_, with)]) = <[(String, ItemMod); 2]>::try_from(copies) else {
        unreachable!("two guards, two copies");
    };
    for (copy, archive, methods) in [
        (&without, false, ["add", "helper"].as_slice()),
        (
            &with,
            true,
            ["add", "archive", "helper", "count"].as_slice(),
        ),
    ] {
        assert_eq!(functions(copy), methods);
        let tokens = quote!(#copy).to_string();
        assert_eq!(
            tokens.matches("# [cfg (feature = \"archive\")]").count(),
            1,
            "{tokens}"
        );
        assert!(
            tokens.contains("# [cfg (feature = \"archive\")] pub shelf : String"),
            "{tokens}"
        );
        assert!(tokens.contains("# [cfg (test)] fn helper ()"), "{tokens}");
        assert_eq!(tokens.contains("pub struct Shelf ;"), archive, "{tokens}");
        let again = expansion(attribute(), quote!(#copy)).to_string();
        assert!(again.contains("pub const WIT"), "{again}");
        assert!(!again.contains(":: tembra :: orb"), "{again}");
        assert!(!again.contains("__tembra_configuration"), "{again}");
    }
    let (without, with) = (expanded(&without), expanded(&with));
    let wit = |module: &ItemMod| constant(module, "WIT").expect("the WIT");
    assert!(!wit(&without).contains("archive"), "{}", wit(&without));
    assert!(
        !wit(&without).contains("interface shelf"),
        "{}",
        wit(&without)
    );
    assert!(
        wit(&with).contains("archive: func() -> u32;"),
        "{}",
        wit(&with)
    );
    assert!(wit(&with).contains("interface shelf {"), "{}", wit(&with));
    let notes = |module: &ItemMod| constant(module, "__TEMBRA_SIGNATURE_NOTES").unwrap();
    assert_ne!(notes(&without), notes(&with));
    assert_eq!(constant(&without, "__TEMBRA_SIGNATURE_SHELF"), None);
    assert!(constant(&with, "__TEMBRA_SIGNATURE_SHELF").is_some());
    for (module, present) in [(&without, false), (&with, true)] {
        let tokens = quote!(#module).to_string();
        for registered in [
            "fn archive () -> u32 { Notes :: archive () }",
            ". method_with (\"archive\"",
            ". service (self :: __tembra_service_shelf (name))",
        ] {
            assert_eq!(
                tokens.contains(registered),
                present,
                "{registered}\n{tokens}"
            );
        }
    }
}

/// With two predicates, four copies, one per combination: a method under both `#[cfg]`s stays
/// only where both hold, an impl block's `#[cfg]` takes its methods with it, and a `#[create]`
/// under a `#[cfg]` builds the service's objects only where it holds. Every copy expands.
#[test]
fn each_copy_keeps_the_items_whose_predicates_hold() {
    let copies = copies(
        r#"mod notebook {
            #[inputs]
            pub struct Inputs {
                pub label: String,
            }

            #[service]
            pub struct Notes;

            impl Notes {
                pub fn always() -> u32 {
                    0
                }

                #[cfg(feature = "a")]
                pub fn first() -> u32 {
                    1
                }

                #[cfg(feature = "a")]
                #[cfg(feature = "b")]
                pub fn both() -> u32 {
                    2
                }

                #[cfg(feature = "b")]
                #[create]
                fn create(_: &Inputs, _: &tembra::Declaration) -> tembra::Result<Self> {
                    Ok(Self)
                }
            }

            #[cfg(feature = "b")]
            impl Notes {
                pub fn second() -> u32 {
                    3
                }
            }
        }"#,
    );
    let read: Vec<(&str, Vec<String>)> = copies
        .iter()
        .map(|(guard, copy)| (guard.as_str(), functions(copy)))
        .collect();
    let functions = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
    assert_eq!(
        read,
        [
            (
                "all (not (feature = \"a\") , not (feature = \"b\"))",
                functions(&["always"])
            ),
            (
                "all (feature = \"a\" , not (feature = \"b\"))",
                functions(&["always", "first"])
            ),
            (
                "all (not (feature = \"a\") , feature = \"b\")",
                functions(&["always", "create", "second"])
            ),
            (
                "all (feature = \"a\" , feature = \"b\")",
                functions(&["always", "first", "both", "create", "second"])
            ),
        ]
    );
    for (guard, copy) in &copies {
        let tokens = quote!(#copy).to_string();
        assert!(!tokens.contains("cfg"), "{tokens}");
        let module = expanded(copy);
        let module = quote!(#module).to_string();
        let built = guard.ends_with(", feature = \"b\")");
        assert_eq!(
            module.contains("self :: __tembra_service_notes (name , & inputs)"),
            built,
            "{guard}\n{module}"
        );
    }
}

/// Each distinct predicate doubles the copies: four make sixteen, and a fifth, wherever the
/// macro reads it, is refused at its `#[cfg]`, with the runtime prelude of a freestanding guest,
/// as every refusal of the module. A predicate written twice is one, and so is one under `not`.
#[test]
fn more_than_four_predicates_are_refused() {
    let module = |fifth: &str| {
        format!(
            r#"mod notebook {{
                #[service]
                pub struct Notes;

                impl Notes {{
                    #[cfg(feature = "a")]
                    pub fn a() {{}}
                    #[cfg(feature = "b")]
                    pub fn b() {{}}
                    #[cfg(feature = "c")]
                    pub fn c() {{}}
                    #[cfg(feature = "a")]
                    pub fn again() {{}}
                    #[cfg(not(feature = "a"))]
                    pub fn otherwise() {{}}
                    pub fn always() {{}}
                }}

                #[cfg({fifth})]
                #[service]
                pub struct Shelf;

                #[cfg({fifth})]
                impl Shelf {{
                    pub fn count() {{}}
                }}
            }}"#
        )
    };
    let copies = copies(&module("feature = \"d\""));
    assert_eq!(copies.len(), 16);
    let guards: std::collections::BTreeSet<&String> =
        copies.iter().map(|(guard, _)| guard).collect();
    assert_eq!(guards.len(), 16);
    let shelves = copies
        .iter()
        .filter(|(_, copy)| functions(copy).contains(&"count".to_owned()))
        .count();
    assert_eq!(shelves, 8);
    let fifth = module("any(feature = \"d\", feature = \"e\")").replace(
        "pub fn always",
        "#[cfg(feature = \"e\")] pub fn e() {} pub fn always",
    );
    for (tail, prelude) in [(quote!(), 0), (quote!(, no_std), 1)] {
        let attribute = quote!(package = "example:notes@1.0.0" #tail);
        let refused = expansion(attribute, syn::parse_str(&fifth).unwrap()).to_string();
        assert_eq!(refused.matches("compile_error").count(), 1, "{refused}");
        assert!(
            refused.contains(
                "more than 4 distinct #[cfg] predicates on the module's services, methods, root and hooks: the macro expands the module once per combination of them, and combines at most 4; gate these items under fewer predicates, such as one feature"
            ),
            "{refused}"
        );
        assert!(!refused.contains(":: tembra :: orb"), "{refused}");
        assert_eq!(
            refused.matches("# [global_allocator]").count(),
            prelude,
            "{refused}"
        );
    }
}

/// A module whose `#[cfg]`s are all on items the macro does not read expands at once, the
/// `#[cfg]`s left as written for the compiler: on a field of the `#[inputs]` struct, on a type, on
/// a function of a service that is no method, on a sub-module holding no service, root or hook,
/// such as the module's tests, or a malformed one the compiler reports.
#[test]
fn a_module_without_such_a_cfg_expands_at_once() {
    let output = expansion(
        attribute(),
        syn::parse_str(
            r#"mod notebook {
                #[inputs]
                pub struct Inputs {
                    #[cfg(feature = "archive")]
                    pub shelf: String,
                }

                #[cfg(feature = "archive")]
                pub struct Archive {
                    pub notes: Vec<String>,
                }

                #[service]
                pub struct Notes;

                impl Notes {
                    pub fn add(text: String) -> u32 {
                        text.len() as u32
                    }

                    #[cfg(feature = "archive")]
                    pub(crate) fn archived() -> Archive {
                        Archive { notes: Vec::new() }
                    }

                    #[cfg]
                    pub fn malformed() {}
                }

                #[cfg(test)]
                mod tests {
                    #[test]
                    fn adds() {
                        assert_eq!(super::Notes::add("a".into()), 1);
                    }
                }
            }"#,
        )
        .unwrap(),
    )
    .to_string();
    assert!(output.contains("pub const WIT"), "{output}");
    assert!(!output.contains(":: tembra :: orb"), "{output}");
    for kept in [
        "# [cfg (feature = \"archive\")] pub shelf : String",
        "# [cfg (feature = \"archive\")] pub struct Archive",
        "# [cfg (feature = \"archive\")] pub (crate) fn archived ()",
        "# [cfg] pub fn malformed ()",
        "# [cfg (test)] mod tests",
    ] {
        assert!(output.contains(kept), "{kept}\n{output}");
    }
}

/// A sub-module that holds a service, here or deeper, is the service's place too: its `#[cfg]`
/// takes the services with it, out of the WIT and of `orb(..)`, while a sub-module holding none,
/// such as the module's tests, keeps its `#[cfg]` for the compiler.
#[test]
fn a_sub_module_holding_a_service_follows_its_cfg() {
    let copies = copies(
        r#"mod notebook {
            #[service]
            pub struct Notes;

            impl Notes {
                pub fn add(text: String) -> u32 {
                    text.len() as u32
                }
            }

            #[cfg(feature = "archive")]
            pub mod shelf {
                #[service]
                pub struct Shelf;

                impl Shelf {
                    pub fn count() -> u32 {
                        0
                    }
                }

                pub mod rack {
                    #[service]
                    pub struct Rack;

                    impl Rack {
                        pub fn size() -> u32 {
                            0
                        }
                    }
                }
            }

            #[cfg(test)]
            mod tests {
                #[test]
                fn adds() {}
            }
        }"#,
    );
    let guards: Vec<&str> = copies.iter().map(|(guard, _)| guard.as_str()).collect();
    assert_eq!(
        guards,
        [
            "all (not (feature = \"archive\"))",
            "all (feature = \"archive\")"
        ]
    );
    for ((_, copy), archive) in copies.iter().zip([false, true]) {
        let tokens = quote!(#copy).to_string();
        assert!(!tokens.contains("# [cfg (feature"), "{tokens}");
        assert_eq!(tokens.contains("pub mod shelf"), archive, "{tokens}");
        assert!(tokens.contains("# [cfg (test)] mod tests"), "{tokens}");
        let module = expanded(copy);
        let wit = constant(&module, "WIT").expect("the WIT");
        for interface in ["interface shelf {", "interface rack {"] {
            assert_eq!(wit.contains(interface), archive, "{interface}\n{wit}");
        }
        let tokens = quote!(#module).to_string();
        for registered in [
            "__tembra_service_shelf (name)",
            "__tembra_service_rack (name)",
        ] {
            assert_eq!(
                tokens.contains(registered),
                archive,
                "{registered}\n{tokens}"
            );
        }
    }
}

/// A `#[root]` under a `#[cfg]` is the Orb's create only where its predicate holds: elsewhere,
/// `orb(..)` hands `Orb::create` nothing.
#[test]
fn a_root_follows_its_cfg() {
    let copies = copies(
        r#"mod worker {
            #[inputs]
            pub struct Inputs {
                pub label: String,
            }

            #[service]
            pub struct Notes;

            impl Notes {
                pub fn add(text: String) -> u32 {
                    text.len() as u32
                }
            }

            #[cfg(feature = "worker")]
            #[root]
            fn start(_: &Inputs, _: &tembra::Declaration) -> tembra::Result<()> {
                Ok(())
            }
        }"#,
    );
    for ((guard, copy), worker) in copies.iter().zip([false, true]) {
        let tokens = quote!(#copy).to_string();
        assert_eq!(tokens.contains("fn start"), worker, "{guard}\n{tokens}");
        assert!(!tokens.contains("# [cfg"), "{tokens}");
        let module = expanded(copy);
        let module = quote!(#module).to_string();
        assert_eq!(
            module.contains("move | declaration | self :: start (& inputs , & declaration)"),
            worker,
            "{guard}\n{module}"
        );
    }
}

/// The WIT package of the hook `example:admission/guard@1.0.0`, in a directory of the system's
/// temporary directory.
fn admission() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tembra-orb-macro-cfg-admission-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.wit"),
        "package example:admission@1.0.0;
interface guard {
  record admission { value: s32, blocked: bool }
  apply: func(input: admission) -> admission;
}
",
    )
    .unwrap();
    dir
}

/// A function or a struct marked `#[hook]` under a `#[cfg]` is in the WIT only where its predicate
/// holds: a contribution, whose hook the world exports, and a declaration, whose derived interface
/// it is.
#[test]
fn a_hook_follows_its_cfg() {
    let dir = admission().display().to_string();
    let attribute = quote!(package = "example:gate@1.0.0", wit = #dir);
    let copies = copies_with(
        attribute.clone(),
        r#"mod gate {
            #[service]
            pub struct Ledger;

            impl Ledger {
                pub fn count() -> u32 {
                    0
                }
            }

            #[cfg(feature = "audit")]
            #[hook("example:admission/guard@1.0.0", priority = 1)]
            pub fn admit(input: guard::Admission) -> guard::Admission {
                input
            }

            #[cfg(feature = "audit")]
            #[hook(composition = series, input = u32)]
            pub struct Audit;
        }"#,
    );
    for ((guard, copy), audit) in copies.iter().zip([false, true]) {
        let tokens = quote!(#copy).to_string();
        for item in ["pub fn admit", "pub struct Audit"] {
            assert_eq!(tokens.contains(item), audit, "{guard}\n{tokens}");
        }
        let module = expanded_with(attribute.clone(), copy);
        let wit = constant(&module, "WIT").expect("the WIT");
        for line in ["export example:admission/guard@1.0.0;", "interface audit {"] {
            assert_eq!(wit.contains(line), audit, "{line}\n{wit}");
        }
    }
}

/// A predicate under `not(..)` is the same predicate, its answer turned over: `x` and `not(x)`
/// make two copies, not four.
#[test]
fn a_negated_predicate_is_the_same_predicate() {
    let read: Vec<(String, Vec<String>)> = copies(
        r#"mod notebook {
            #[service]
            pub struct Notes;

            impl Notes {
                #[cfg(feature = "a")]
                pub fn on() -> u32 {
                    1
                }

                #[cfg(not(feature = "a"))]
                pub fn off() -> u32 {
                    0
                }

                #[cfg(not(not(feature = "a")))]
                pub fn again() -> u32 {
                    1
                }
            }
        }"#,
    )
    .iter()
    .map(|(guard, copy)| (guard.clone(), functions(copy)))
    .collect();
    let copy = |guard: &str, names: &[&str]| {
        (
            guard.to_owned(),
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        read,
        [
            copy("all (not (feature = \"a\"))", &["off"]),
            copy("all (feature = \"a\")", &["on", "again"]),
        ]
    );
}

/// A copy left with nothing to export, or, natively, nothing to run either, says so with the
/// predicate of its combination: what its `#[cfg]`s left out. The copy where they hold expands.
#[test]
fn nothing_to_export_names_the_configuration() {
    for (module, message) in [
        (
            r#"mod notebook {
                #[cfg(feature = "archive")]
                #[service]
                pub struct Shelf;

                #[cfg(feature = "archive")]
                impl Shelf {
                    pub fn count() -> u32 {
                        0
                    }
                }
            }"#,
            "nothing to export: mark a unit struct with #[service] and give it an impl block of pub fn, or mark a hook with #[hook]; the module's #[cfg]s leave none under #[cfg(all (not (feature = \"archive\")))], the configuration of this build",
        ),
        (
            r#"mod worker {
                #[inputs]
                pub struct Inputs {
                    pub label: String,
                }

                #[cfg(feature = "worker")]
                #[root]
                fn start(_: &Inputs, _: &tembra::Declaration) -> tembra::Result<()> {
                    Ok(())
                }
            }"#,
            "nothing to export or run: mark a unit struct with #[service] and give it an impl block of pub fn, mark a hook with #[hook], or give the Orb a function marked #[root]; the module's #[cfg]s leave none under #[cfg(all (not (feature = \"worker\")))], the configuration of this build",
        ),
    ] {
        let Ok([(_, without), (_, with)]) = <[(String, ItemMod); 2]>::try_from(copies(module))
        else {
            unreachable!("one predicate, two copies");
        };
        let options = || options(attribute()).unwrap();
        let error = expand(options(), without, Metadata::named("calculator"), None)
            .unwrap_err()
            .to_string();
        assert_eq!(error, message);
        expand(options(), with, Metadata::named("calculator"), None)
            .unwrap_or_else(|error| panic!("{error}"));
    }
}
