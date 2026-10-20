//! Emit a portable descriptor from typed Rust declarations, including methods and state.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tembra_contracts::{ContractBuilder, Placement};

#[derive(Serialize, Deserialize, JsonSchema)]
struct CounterInput {
    name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Increment {
    by: i32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let contract = ContractBuilder::remote("fixture.typed-counter", 1)
        .input::<CounterInput>()?
        .placement(Placement::Named {
            name: "service".into(),
        })
        .select_variant("default")
        .typed_method::<Increment, i32>("increment")?
        .typed_query::<i32>("read")?
        .typed_state::<i32>("count")?
        .method("touch", None, None)
        .build()?;
    println!("{}", serde_json::to_string_pretty(&contract)?);
    Ok(())
}
