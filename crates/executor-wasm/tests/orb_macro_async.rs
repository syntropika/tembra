//! The macro's fixed WIT bridge really suspends and resumes across a native import.
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc, oneshot},
    executor::LocalPool,
    task::LocalSpawnExt,
};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use tembra::{Host, Orb, Service, portable::Owned};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const CLOCK: &str = "example:pace/clock@1.0.0";
const NO_ARGUMENTS: [u8; 0] = [];
const FEED: &str = "fixture::example:events/feed@1.0.0";

fn contract(world: &str) -> Contract {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../orb-macro/test-wit"),
        Some(world),
    )
    .unwrap()
}

struct Pace {
    entered: mpsc::UnboundedSender<u64>,
    calls: RefCell<Vec<u64>>,
    gates: RefCell<VecDeque<oneshot::Sender<()>>>,
}

fn pacer(pace: Rc<Pace>) -> Orb {
    let service = contract("example:pace/pacer@1.0.0")
        .plan("pace", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("pace", 1).service(
        Service::new(service.name, move |_| Ok(pace.clone()))
            .signature(service.signature)
            .method("wait", |pace: Rc<Rc<Pace>>, _, (value,): (u64,)| {
                let (release, gate) = oneshot::channel();
                pace.calls.borrow_mut().push(value);
                pace.gates.borrow_mut().push_back(release);
                pace.entered.unbounded_send(value).unwrap();
                async move {
                    gate.await
                        .expect("the test must release every pending call");
                    Ok(value * 10)
                }
            }),
    )
}

#[test]
#[ignore = "requires the compiled async macro fixture; run just test-orb-macro"]
fn fixed_wit_async_functions_and_resource_methods_suspend_and_resume() {
    let path = std::env::var_os("TEMBRA_WASM_ORB_MACRO_ASYNC")
        .expect("TEMBRA_WASM_ORB_MACRO_ASYNC must name the compiled async fixture");
    let bytes = std::fs::read(path).unwrap();
    for interpreted in [false, true] {
        let limits = Limits::default();
        let engine = if interpreted {
            Engine::interpreted(limits)
        } else {
            Engine::new(limits)
        }
        .unwrap();
        let artifact = contract("example:events/events@1.0.0")
            .component_artifact(&bytes, "fixture", 1, &Default::default())
            .unwrap();
        let component = engine
            .load(&artifact, Install::default().requires(CLOCK, "pace", 1))
            .unwrap();
        let mut pool = LocalPool::new();
        let host = Rc::new(Host::new(pool.spawner()));
        let (entered, mut arrivals) = mpsc::unbounded();
        let pace = Rc::new(Pace {
            entered,
            calls: RefCell::default(),
            gates: RefCell::default(),
        });
        host.install(vec![pacer(pace.clone()), component]).unwrap();
        let feed = host.get(FEED, vec!["main".into()], "alice").unwrap();
        pool.run_until(feed.ready()).unwrap();
        let (finished, mut outcome) = oneshot::channel();
        let task_host = host.clone();
        let operation = async move {
            task_host
                .root("alice", move |context| {
                    async move {
                        assert_eq!(
                            feed.call_portable::<_, u32>(&context, "count", NO_ARGUMENTS)
                                .await?,
                            1
                        );
                        let updates = feed
                            .call_portable::<_, Owned>(&context, "subscribe", NO_ARGUMENTS)
                            .await?;
                        let first = updates
                            .call_portable::<_, Option<u64>>(
                                &context,
                                "[method]updates.next",
                                NO_ARGUMENTS,
                            )
                            .await?;
                        let fresh = feed
                            .call_portable::<_, Owned>(
                                &context,
                                "[static]updates.fresh",
                                NO_ARGUMENTS,
                            )
                            .await?;
                        let second = fresh
                            .call_portable::<_, Option<u64>>(
                                &context,
                                "[method]updates.next",
                                NO_ARGUMENTS,
                            )
                            .await?;
                        let constructed = feed
                            .call_portable::<_, Owned>(
                                &context,
                                "[constructor]updates",
                                NO_ARGUMENTS,
                            )
                            .await?;
                        drop(constructed);
                        drop(fresh);
                        drop(updates);
                        Ok((first, second))
                    }
                    .boxed_local()
                })
                .await
        };
        pool.spawner()
            .spawn_local(async move {
                let result = operation.await;
                let _ = finished.send(result);
            })
            .unwrap();

        let expected = [7, 71, 1, 11];
        for index in 0..expected.len() {
            match pool.run_until(futures::future::select(arrivals.next(), &mut outcome)) {
                futures::future::Either::Left((value, _)) => {
                    assert_eq!(value, Some(expected[index]))
                }
                futures::future::Either::Right((result, _)) => {
                    panic!("operation ended before pending import: {result:?}")
                }
            }
            pool.run_until_stalled();
            assert!(
                outcome.try_recv().unwrap().is_none(),
                "call completed before host release"
            );
            assert_eq!(
                *pace.calls.borrow(),
                expected[..=index],
                "wrong suspended import"
            );
            assert_eq!(pace.gates.borrow().len(), 1);
            pace.gates
                .borrow_mut()
                .pop_front()
                .unwrap()
                .send(())
                .unwrap();
        }
        let result = pool.run_until(outcome).unwrap().unwrap();
        assert_eq!(result, (Some(710), Some(110)));
        pool.run_until(host.settled());
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        let diagnostics = host.kernel().diagnostics();
        assert_eq!(
            (diagnostics.calls, diagnostics.references, diagnostics.roots),
            (0, 0, 0)
        );
        assert!(host.errors().is_empty());
    }
}
