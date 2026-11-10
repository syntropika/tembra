//! Resolve the handler contract, including the bundled standard `wasi:http` package, and let the
//! official macro generate Rust bindings for the selected world.
fn main() {
    let directory = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let contract = directory.join("../tests/wit/handler");
    println!(
        "cargo:rerun-if-changed={}",
        contract.join("handler.wit").display()
    );
    let world = if std::env::var_os("CARGO_FEATURE_PROXY").is_some() {
        "proxy"
    } else {
        "handler"
    };
    let resolved = tembra_contracts::Contract::from_dir(contract, Some(world)).unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let package = output.join("contract.wasm");
    std::fs::write(&package, resolved.encode().unwrap()).unwrap();
    std::fs::write(
        output.join("bindings.rs"),
        format!(
            "wit_bindgen::generate!({{path:{:?},world:{world:?},generate_all}});",
            package.to_str().unwrap()
        ),
    )
    .unwrap();
}
