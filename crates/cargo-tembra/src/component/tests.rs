//! Reading a component: the verdict of `inspect` against a WIT, and how the summary accounts for
//! the component's imports and exports.
use super::{Import, classify_import, consumed_packages, inspect, is_ingress, world_key};
use crate::scratch;
use wit_parser::Resolve;

/// The smallest component: the header alone, importing and exporting nothing.
const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\x00\x01\x00";

/// The standard HTTP package as far as the handler goes: its exact identity and shape.
const HTTP: &str = "package wasi:http@0.3.0 {
  interface types {
    resource request;
    resource response;
    variant error-code { internal }
  }
  interface handler {
    use types.{request, response, error-code};
    handle: async func(request: request) -> result<response, error-code>;
  }
}
";

#[test]
fn imports_are_classified_by_the_rules_of_installation() {
    const UNSUPPORTED: Import = Import::Wasi("NOT SUPPORTED by Tembra");
    let classify = |id| classify_import(id, false);
    assert_eq!(classify("example:greeter/greeting@1.0.0"), Import::Orb);
    let filesystem = Import::Wasi("filesystem (FilesystemGrant)");
    assert_eq!(classify("wasi:filesystem/preopens@0.3.0"), filesystem);
    let network = Import::Wasi("network (NetworkGrant)");
    assert_eq!(classify("wasi:sockets/types@0.3.0"), network);
    // The interfaces `system_capability` admits, and no other of their packages.
    let clocks = Import::Wasi("system: clocks");
    assert_eq!(classify("wasi:clocks/monotonic-clock@0.3.0"), clocks);
    let random = Import::Wasi("system: random");
    assert_eq!(classify("wasi:random/insecure-seed@0.3.0"), random);
    assert_eq!(
        classify("wasi:cli/stdout@0.3.0"),
        Import::Wasi("system: cli")
    );
    assert_eq!(classify("wasi:clocks/timezone@0.3.0"), UNSUPPORTED);
    assert_eq!(classify("wasi:clocks/monotonic-clock@0.2.0"), UNSUPPORTED);
    assert_eq!(classify("wasi:cli/run@0.3.0"), UNSUPPORTED);
    assert_eq!(classify("wasi:keyvalue/store@0.2.0"), UNSUPPORTED);
    // The HTTP types need the client's grant, unless the Orb only serves incoming HTTP.
    let http = Import::Wasi("http (HttpGrant)");
    assert_eq!(classify("wasi:http/types@0.3.0"), http);
    assert_eq!(classify("wasi:http/client@0.3.0"), http);
    assert_eq!(
        classify_import("wasi:http/types@0.3.0", true),
        Import::Ingress
    );
}

#[test]
fn the_incoming_http_handler_is_the_exact_standard_export() {
    let source = format!(
        "package example:web@1.0.0;

interface handler {{
  use wasi:http/types@0.3.0.{{request, response, error-code}};
  handle: async func(request: request) -> result<response, error-code>;
}}

interface api {{
  handle: func(request: string) -> string;
}}

world web {{
  export wasi:http/handler@0.3.0;
  export handler;
  export api;
}}

{HTTP}"
    );
    let mut resolve = Resolve::default();
    let package = resolve.push_str("web.wit", &source).unwrap();
    let world = resolve.select_world(&[package], Some("web")).unwrap();
    let ingress: Vec<_> = resolve.worlds[world]
        .exports
        .keys()
        .map(|key| (world_key(&resolve, key, None), is_ingress(&resolve, key)))
        .collect();
    // An interface of the same shape under another identity is an ordinary export.
    assert_eq!(
        ingress,
        [
            ("wasi:http/handler@0.3.0".to_owned(), true),
            ("example:web/handler@1.0.0".to_owned(), false),
            ("example:web/api@1.0.0".to_owned(), false),
        ]
    );
}

/// The contract another Orb's build wrote: its package, the one it imports nested whole, whose
/// world imports the kernel's state.
const SECOND: &str = "package example:second@1.0.0;

interface relay {
  twice: func() -> u32;
}

world second {
  import example:first/tally@1.0.0;
  export relay;
}

package example:first@1.0.0 {
  interface tally {
    bump: func() -> u32;
    reset: func();
  }
  world first {
    import tembra:guest/state@1.0.0;
    export tembra:guest/state-init@1.0.0;
    export tally;
  }
}
";

#[test]
fn a_consumed_package_brings_the_packages_it_builds_on() {
    // The consumed directory, read whole: the other contract and the kernel's package it names.
    let guest = tembra_contracts::Contract::from_source("second.wit", SECOND, Some("second"))
        .unwrap()
        .standard_dependencies();
    let mut whole = Resolve::default();
    for (name, source) in guest {
        whole.push_str(name, source).unwrap();
    }
    whole.push_str("second.wit", SECOND).unwrap();
    // The component's own types reach the imported package alone, and only what it calls.
    let mut types = Resolve::default();
    types
        .push_str(
            "second.wit",
            "package example:second@1.0.0; interface relay { twice: func() -> u32; }",
        )
        .unwrap();
    types
        .push_str(
            "third.wit",
            "package example:third@1.0.0; interface caller { call: func() -> u32; }",
        )
        .unwrap();
    let root = types
        .push_str(
            "root.wit",
            "package root:component; world root { import example:second/relay@1.0.0; export example:third/caller@1.0.0; }",
        )
        .unwrap();
    let own = "package example:third@1.0.0;\n\ninterface caller {\n  call: func() -> u32;\n}\n\n\
               world third {\n  import example:second/relay@1.0.0;\n  export caller;\n}\n";
    let consumed = consumed_packages(&types, root, "example:third@1.0.0", own, Some(&whole));
    // Each from the consumed directory, whole; the kernel's package is never nested.
    let names: Vec<String> = consumed
        .iter()
        .map(|(source, id)| {
            assert!(std::ptr::eq(*source, &whole));
            source.packages[*id].name.to_string()
        })
        .collect();
    assert_eq!(names, ["example:second@1.0.0", "example:first@1.0.0"]);
    // Without the consumed directory, the component's types are all there is.
    let consumed = consumed_packages(&types, root, "example:third@1.0.0", own, None);
    assert_eq!(consumed.len(), 1);
    assert_eq!(
        types.packages[consumed[0].1]
            .interfaces
            .keys()
            .collect::<Vec<_>>(),
        ["relay"]
    );
}

#[test]
fn inspect_fails_when_the_verifier_refuses_the_component() {
    let dir = scratch("inspect-verdict");
    let component = dir.join("empty.wasm");
    std::fs::write(&component, EMPTY_COMPONENT).unwrap();
    let wit = |name: &str, source: &str| {
        let wit = dir.join(name);
        std::fs::create_dir_all(&wit).unwrap();
        std::fs::write(wit.join(format!("{name}.wit")), source).unwrap();
        wit
    };
    // A world with nothing to export accepts the empty component.
    let empty = wit("empty", "package example:empty@1.0.0;\n\nworld empty {}\n");
    inspect(&component, Some(empty), Some("empty".into())).unwrap();
    // A world exporting a service refuses it, and the command fails.
    let greeter = wit(
        "greeter",
        "package example:greeter@1.0.0;\n\ninterface greeting {\n  hello: func(name: string) -> string;\n}\n\n\
         world greeter {\n  export greeting;\n}\n",
    );
    let error = inspect(&component, Some(greeter.clone()), Some("greeter".into())).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "the component does not satisfy world greeter of {}",
            greeter.display()
        )
    );
    std::fs::remove_dir_all(dir).unwrap();
}
