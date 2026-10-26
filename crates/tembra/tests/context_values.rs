use futures::{FutureExt, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Context, Facet, Host, Service, core::ContextKey};

const TRACE: ContextKey<String> = ContextKey::new("test/trace", 1);

#[test]
fn native_calls_keep_their_frame_and_end_its_invocation_authority() {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    let captured = Rc::new(RefCell::new(Vec::<Context>::new()));
    let retained = captured.clone();
    let service = Service::new("echo", move |_| Ok(retained.clone())).method(
        "read",
        |captured, ctx, (): ()| async move {
            captured
                .borrow_mut()
                .push(ctx.with(&TRACE, "inside".into())?);
            ctx.value(&TRACE)
        },
    );
    host.install(vec![Facet::new("context", 1).service(service)])
        .unwrap();
    let reference = Rc::new(host.get("echo", vec!["main".into()], "alice").unwrap());
    executor
        .run_until(host.root("alice", {
            let reference = reference.clone();
            move |root| {
                async move {
                    let left = root.with(&TRACE, "left".into())?;
                    let right = root.with(&TRACE, "right".into())?;
                    let (first, second) = futures::join!(
                        reference.call::<_, Option<String>>(&left, "read", ()),
                        reference.call::<_, Option<String>>(&right, "read", ())
                    );
                    assert_eq!(first?.as_deref(), Some("left"));
                    assert_eq!(second?.as_deref(), Some("right"));
                    assert_eq!(root.value(&TRACE)?, None);
                    assert_eq!(left.principal()?, "alice");
                    assert!(captured.borrow()[0].with(&TRACE, "late".into()).is_err());
                    Ok(())
                }
                .boxed_local()
            }
        }))
        .unwrap();
    assert_eq!(host.kernel().context_stats().0, 0);
    reference.release().unwrap();
    executor.run_until(host.close()).unwrap();
}
