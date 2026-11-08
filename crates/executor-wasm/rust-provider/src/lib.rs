//! A compiled-language Orb consumes the same WIT package as its TypeScript caller.
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
struct Arithmetic;
impl exports::example::integrated::arithmetic::Guest for Arithmetic {
    /// Execute arithmetic inside the component, not in the embedding host.
    fn double(value: i32) -> i32 {
        value * 2
    }
}
export!(Arithmetic);
