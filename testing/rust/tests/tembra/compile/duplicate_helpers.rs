#[tembra::contract(local,name="compile.duplicate-helper",version=1)] trait Invalid { #[portable] #[portable] async fn read(&self)->tembra::Result<u32>; }
