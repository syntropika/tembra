#[tembra::contract(local,name="compile.conflict-helper",version=1)] trait Invalid { #[opaque(id="value@1")] #[portable] fn value(&self)->u32; }
