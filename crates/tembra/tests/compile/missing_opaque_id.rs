#[tembra::contract(local,name="compile.opaque-id",version=1)] trait Invalid { #[opaque()] fn value(&self)->u32; }
