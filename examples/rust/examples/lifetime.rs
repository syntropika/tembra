//! What happens to an object after its last claim is released: an immediate reacquisition
//! rescues it with its state intact, a later one finds it disposed and rebuilds it.

#[path = "../helpers/scheduler.rs"]
mod scheduler;

use scheduler::let_scheduler_run;
use tembra::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(crate = "tembra::serde")]
#[schemars(crate = "tembra::schemars")]
struct Named {
    name: String,
}

#[tembra::contract(local, name = "example.lifetime-counter", version = 1, input = Named)]
trait Counter {
    #[state]
    fn count(&self) -> u32;
    async fn increment(&self) -> Result<u32>;
}

struct CounterService {
    label: String,
    count: Cell<u32>,
}

// 1. The provider struct dies with its object. The drop shows the exact moment of disposal.
impl Drop for CounterService {
    fn drop(&mut self) {
        println!(
            "      [DROP] CounterService '{}' destroyed: the object was disposed",
            self.label
        );
    }
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
        self.count.value()
    }
}

fn main() -> Result<()> {
    tembra::native::run("lifetime-example", (), |host| async move {
        let facet = NativeFacet::new("counters", vec![], |_, setup| {
            let count = setup.state::<u32>("count")?;
            setup.serve(Counter, move |input: Named, principal, declaration| {
                println!(
                    "    [factory] new CounterService for {:?} / {principal}",
                    input.name
                );
                Ok(CounterService {
                    label: format!("{}/{principal}", input.name),
                    count: declaration.state(count.value(0)?)?,
                })
            })
        })?;
        host.install(facet, Default::default())?.await?;
        let alice = || tembra::GetOptions {
            principal: Some(tembra::serde_json::json!("alice")),
            state: true,
        };
        let visits = || Named {
            name: "visits".into(),
        };

        println!("=== 1. acquire alice's counter and increment it three times ===");
        let first = host.get_with_options(Counter, visits(), alice()).await?;
        for _ in 0..3 {
            first.increment().await?;
        }
        println!(
            "    count = {}  object_id = {}",
            first.count().value()?,
            first.core().object_id()
        );

        println!("=== 2. drop it and reacquire IMMEDIATELY, without yielding to the scheduler ===");
        drop(first);
        let second = host.get_with_options(Counter, visits(), alice()).await?;
        println!(
            "    count = {}  object_id = {}   (no [factory], no [DROP]: the pending disposal was cancelled)",
            second.count().value()?,
            second.core().object_id()
        );

        println!("=== 3. drop it, then let the scheduler run the pending disposal ===");
        drop(second);
        let_scheduler_run().await;

        println!("=== 4. acquire alice's counter again ===");
        let third = host.get_with_options(Counter, visits(), alice()).await?;
        println!(
            "    count = {}  object_id = {}",
            third.count().value()?,
            third.core().object_id()
        );
        drop(third);
        Ok(())
    })
}
