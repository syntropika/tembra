#[tembra::contract(local,name="compile.malformed",version=1)] trait Invalid { #[state(foo)] fn count(&self)->u32; }
