//! Let Cargo supply the contract output and track additions to canonical package directories.
fn main() {
    for source in ["build.rs", "wit"] {
        println!("cargo:rerun-if-changed={source}");
    }
}
