//! Build-time generation of source WASI bindings from the bundled standard contracts.
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use wit_parser::Resolve;

mod codegen;

fn main() {
    let wit = PathBuf::from("../contracts/wit/wasi03");
    let mut resolve = Resolve::default();
    for name in ["clocks", "random", "filesystem", "sockets", "cli"] {
        let path = wit.join(format!("{name}.wit"));
        println!("cargo:rerun-if-changed={}", path.display());
        let source = std::fs::read_to_string(&path).expect("bundled WASI WIT");
        resolve
            .push_str(&path, &source)
            .expect("bundled WASI WIT resolves");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=codegen");
    let (rust, typescript) = codegen::generate(&resolve);
    // The core import name binds both sides to this exact generated value/method schema.
    let identity = Sha256::digest(typescript.as_bytes());
    println!("cargo:rustc-env=TEMBRA_INTERPRETER_IMPORT_MODULE=tembra-interpreter-v6-{identity:x}");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("standard.rs"), rust).expect("write Rust bindings");
    std::fs::write(out.join("standard.ts"), typescript).expect("write TypeScript table");
}
