//! Register a local counter, acquire a typed reference and call it.

mod counter;

use counter::{Counter, CounterInput};
use tembra::prelude::*;

fn main() -> Result<()> {
    tembra::native::run("example", (), |host| async move {
        counter::serve(&host)?;
        let counter = host.get(Counter, CounterInput {}).await?;
        assert_eq!(counter.read().await?, 0);
        assert_eq!(counter.increment().await?, 1);
        assert_eq!(counter.count().value()?, 1);
        println!("Count: {}", counter.read().await?);
        counter.core().release();
        Ok(())
    })
}
