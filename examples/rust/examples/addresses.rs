//! One service, many objects: the acquisition input and the principal are part of an object's
//! address, so one factory produces independent instances with independent state.

use tembra::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(crate = "tembra::serde")]
#[schemars(crate = "tembra::schemars")]
struct Named {
    name: String,
}

// 1. The input participates in the object's address.
#[tembra::contract(local, name = "example.named-counter", version = 1, input = Named)]
trait Counter {
    #[state]
    fn count(&self) -> u32;
    async fn increment(&self) -> Result<u32>;
}

struct CounterService {
    label: String,
    count: Cell<u32>,
}

impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u32> {
        self.count.readable()
    }

    async fn increment(&self, context: Context) -> Result<u32> {
        self.count.change(&context, |count| {
            *count += 1;
            Ok(())
        })?;
        let value = self.count.value()?;
        println!("      [increment on '{}'] count = {value}", self.label);
        Ok(value)
    }
}

fn main() -> Result<()> {
    tembra::native::run("addresses-example", (), |host| async move {
        // 2. One registered service. Its factory runs once per object, not once per service.
        let facet = NativeFacet::new("counters", vec![], |_, setup| {
            let count = setup.state::<u32>("count")?;
            setup.serve(Counter, move |input: Named, principal, declaration| {
                println!(
                    "    [factory] new CounterService for input={{name:{:?}}} principal={principal}",
                    input.name
                );
                Ok(CounterService {
                    label: input.name,
                    count: declaration.state(count.value(0)?)?,
                })
            })
        })?;
        host.install(facet, Default::default())?.await?;
        println!("=== 1. installed: one recipe, zero objects ===");

        println!("=== 2. get 'visits' and get 'errors': two addresses, two objects ===");
        let visits = host
            .get(
                Counter,
                Named {
                    name: "visits".into(),
                },
            )
            .await?;
        let errors = host
            .get(
                Counter,
                Named {
                    name: "errors".into(),
                },
            )
            .await?;
        println!("    visits object_id = {}", visits.core().object_id());
        println!("    errors object_id = {}", errors.core().object_id());

        println!("=== 3. get 'visits' again: same address, same object, no factory run ===");
        let visits_again = host
            .get(
                Counter,
                Named {
                    name: "visits".into(),
                },
            )
            .await?;
        println!(
            "    visits_again object_id = {}",
            visits_again.core().object_id()
        );

        println!("=== 4. get 'visits' as another principal: another address, another object ===");
        let visits_alice = host
            .get_with_options(
                Counter,
                Named {
                    name: "visits".into(),
                },
                tembra::GetOptions {
                    principal: Some(tembra::serde_json::json!("alice")),
                    state: true,
                },
            )
            .await?;
        println!(
            "    visits_alice object_id = {}",
            visits_alice.core().object_id()
        );

        println!("=== 5. increment differently: every object owns its count ===");
        visits.increment().await?;
        visits.increment().await?;
        errors.increment().await?;
        for _ in 0..3 {
            visits_alice.increment().await?;
        }
        println!("    visits       = {}", visits.count().value()?);
        println!(
            "    visits_again = {}  (same object as visits)",
            visits_again.count().value()?
        );
        println!("    errors       = {}", errors.count().value()?);
        println!("    visits_alice = {}", visits_alice.count().value()?);

        drop((visits, visits_again, errors, visits_alice));
        Ok(())
    })
}
