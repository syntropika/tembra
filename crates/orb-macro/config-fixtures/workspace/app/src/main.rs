//! The application of the workspace, a native host: it declares an Orb of its own with the runtime
//! the facade links by default.

fn main() {
    let _orb = tembra::Orb::new("app", 1);
}
