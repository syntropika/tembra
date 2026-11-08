//! Resolve the same canonical author contract and let the official macro generate Rust bindings.
fn main() {
    let directory = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let contract = directory.join("../../../examples/wasm-orb");
    println!(
        "cargo:rerun-if-changed={}",
        contract.join("contract.wit").display()
    );
    let resolved =
        tembra_contracts::Contract::from_dir(contract, Some("arithmetic-provider")).unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let package = output.join("contract.wasm");
    std::fs::write(&package, resolved.encode().unwrap()).unwrap();
    std::fs::write(
        output.join("bindings.rs"),
        format!(
            "wit_bindgen::generate!({{path:{:?},world:\"arithmetic-provider\"}});",
            package.to_str().unwrap()
        ),
    )
    .unwrap();
}
