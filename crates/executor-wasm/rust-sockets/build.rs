//! Resolve the sockets fixture contract, including the bundled standard `wasi:sockets` package,
//! and let the official macro generate Rust bindings.
fn main() {
    let directory = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let contract = directory.join("../tests/wit/sockets");
    println!(
        "cargo:rerun-if-changed={}",
        contract.join("sockets.wit").display()
    );
    let resolved = tembra_contracts::Contract::from_dir(contract, Some("sockets")).unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let package = output.join("contract.wasm");
    std::fs::write(&package, resolved.encode().unwrap()).unwrap();
    std::fs::write(
        output.join("bindings.rs"),
        format!(
            "wit_bindgen::generate!({{path:{:?},world:\"sockets\",generate_all}});",
            package.to_str().unwrap()
        ),
    )
    .unwrap();
}
