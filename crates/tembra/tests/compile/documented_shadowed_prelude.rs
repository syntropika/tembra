//! Public generated APIs document every item and resist a shadowed standard prelude.
#![deny(missing_docs)]
struct Option;
struct String;
struct Clone;
struct Copy;
struct Debug;
struct Default;
fn Some() {}
fn None() {}
fn Ok() {}
fn Err() {}
macro_rules! format { ($($token:tt)*) => { compile_error!("shadowed format macro") }; }

/// A documented portable service.
#[tembra::contract(remote, name="compile.docs", version=1, host="service", variant="active")]
pub trait Documented {
    /// The current number.
    #[state]
    fn count(&self)->u64;
    async fn add(&self,left:u64,right:u64)->tembra::Result<u64>;
    async fn echo(&self,value: ::std::option::Option<u64>)
        ->tembra::Result<::std::option::Option<u64>>;
}
/// A documented native capability.
#[tembra::contract(local,name="compile.native-docs",version=1)]
pub trait NativeDocs {
    #[opaque(id="value@1")]
    fn value(&self)->u64;
    async fn read(&self)->tembra::Result<u64>;
}
