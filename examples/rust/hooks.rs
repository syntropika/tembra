//! Run a typed extension point with an independently declared contribution.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Hook, Host, Orb, Service, core::Requirement};

fn main() -> tembra::Result<()> {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    let transform = Hook::<String, String>::waterfall("transform");
    let declared = transform.clone();
    let provider = Orb::new("text", 1).hook(transform.clone()).service(
        Service::new("text", move |self_| self_.hook(&declared))
            .method("format", |hook, ctx, text: String| async move {
                hook.run(&ctx, text).await
            }),
    );
    let contributor = Orb::new("uppercase", 1)
        .requires(Requirement {
            orb: "text".into(),
            api_version: 1,
            methods: Default::default(),
            hooks: ["transform".into()].into(),
        })
        .contribute("text", &transform, 0, |_, text| async move {
            Ok(text.to_uppercase())
        });
    host.install(vec![provider, contributor])?;
    let reference = Rc::new(host.get("text", vec!["main".into()], "reader")?);
    let result = executor.run_until(host.root("reader", {
        let reference = reference.clone();
        move |ctx| {
            async move {
                reference
                    .call::<_, String>(&ctx, "format", String::from("hello"))
                    .await
            }
            .boxed_local()
        }
    }))?;
    assert_eq!(result, "HELLO");
    println!("{result}");
    reference.release()?;
    executor.run_until(host.close())
}
