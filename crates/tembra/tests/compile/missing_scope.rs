#[tembra::contract(name = "compile.missing-scope", version = 1)]
trait MissingScope {
    async fn read(&self) -> tembra::Result<u64>;
}
