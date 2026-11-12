//! Reading a component: the verdict of `inspect` against a WIT, and how the summary accounts for
//! the component's imports and exports.
use super::inspect;
use crate::scratch;

/// The smallest component: the header alone, importing and exporting nothing.
const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\x00\x01\x00";

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
