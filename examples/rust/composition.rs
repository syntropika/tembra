//! Share an orb environment, own a required reference, and observe selected native state.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Host, Orb, Service, core::Requirement};

fn main() -> tembra::Result<()> {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    let counter = Orb::new("counter", 1).create(|_| Ok(10_i32)).service(
        Service::new("counter", |self_| {
            self_.state(*self_.orb::<i32>()?, 1, |_| Ok(()))
        })
        .method("increment", |state, ctx, (): ()| async move {
            Ok(*state.update(&ctx, |value| Ok(value + 1))?)
        }),
    );
    let client = Orb::new("client", 1)
        .requires(Requirement {
            orb: "counter".into(),
            api_version: 1,
            methods: [("counter".into(), ["increment".into()].into())].into(),
            hooks: Default::default(),
        })
        .service(
            Service::new("client", |self_| self_.get("counter", vec!["main".into()])).method(
                "increment",
                |counter, ctx, (): ()| async move {
                    counter.call::<_, i32>(&ctx, "increment", ()).await
                },
            ),
        );
    host.install(vec![counter, client])?;
    let counter = host.get("counter", vec!["main".into()], "reader")?;
    executor.run_until(counter.ready())?;
    let _observation = counter
        .state::<i32>()?
        .select(|value| Ok(value % 2))
        .subscribe(|value| {
            println!("Parity: {value}");
            Ok(())
        })?;
    let client = Rc::new(host.get("client", vec!["main".into()], "reader")?);
    assert_eq!(
        executor.run_until(host.root("reader", {
            let client = client.clone();
            move |ctx| {
                async move { client.call::<_, i32>(&ctx, "increment", ()).await }.boxed_local()
            }
        }))?,
        11
    );
    client.release()?;
    counter.release()?;
    executor.run_until(host.close())
}
