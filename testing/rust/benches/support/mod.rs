use futures::{FutureExt, future::poll_fn};
use serde_json::json;
use std::{cell::Cell as Counter, collections::BTreeMap, rc::Rc, task::Poll, time::Instant};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

pub fn options() -> HostOptions {
    let options = HostOptions::new("benchmark", json!(null));
    #[cfg(feature = "observability-bench")]
    let options = match std::env::var("TEMBRA_OBSERVABILITY_SAMPLE_EVERY") {
        Ok(value) => options.with_observability(
            ObservationOptions::new(
                std::env::var("TEMBRA_OBSERVABILITY_CAPACITY")
                    .map_or(131_072, |value| value.parse().expect("positive capacity")),
                value.parse().expect("positive sample interval"),
            )
            .unwrap(),
        ),
        Err(_) => options,
    };
    options
}

pub struct Fixture {
    pub driver: Rc<ManualDriver>,
    pub host: Host,
    pub contract: Rc<Contract>,
    pub reference: Reference,
    pub active: Rc<Counter<usize>>,
    pub maximum: Rc<Counter<usize>>,
    pub registry: RegistryView,
    subscriptions: Vec<Subscription>,
}

pub fn contract() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "benchmark.counter",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "native".into(),
                    Member::Method {
                        args: Some(ValueSchema::Native("u64@1".into())),
                        result: Some(ValueSchema::Native("u64@1".into())),
                    },
                ),
                (
                    "run".into(),
                    Member::Method {
                        args: Some(ValueSchema::Native("u64@1".into())),
                        result: Some(ValueSchema::Native("u64@1".into())),
                    },
                ),
                (
                    "portable".into(),
                    Member::Method {
                        args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                ),
                (
                    "count".into(),
                    Member::State {
                        schema: json!({"type":"integer"}),
                    },
                ),
            ]),
        )
        .unwrap(),
    )
}

struct Active(Rc<Counter<usize>>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl Fixture {
    pub fn new(fanout: usize, pending: bool) -> Self {
        let driver = ManualDriver::new();
        let host = Host::new(options(), driver.clone());
        let contract = contract();
        let registry = Registry::new("benchmark.entries", ConflictPolicy::Error).unwrap();
        let registry_view = host.registry(&registry).unwrap();
        let (active, maximum) = (Rc::new(Counter::new(0)), Rc::new(Counter::new(0)));
        let (inflight, peak) = (active.clone(), maximum.clone());
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                declaration.resource(
                    "benchmark.resource",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                        .with_close(|_| async { Ok(()) }.boxed_local()),
                )?;
                let state = declaration.state(
                    "count",
                    StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                )?;
                declaration.register(&registry, "counter", json!({"kind":"counter"}))?;
                let (inflight, peak) = (inflight.clone(), peak.clone());
                Ok(Implementation::new()
                    .state("count", state.readable())
                    .method("native", |amount: u64, _| {
                        async move { Ok(amount) }.boxed_local()
                    })
                    .method("run", move |amount: u64, context| {
                        let (inflight, peak, state) =
                            (inflight.clone(), peak.clone(), state.clone());
                        async move {
                            inflight.set(inflight.get() + 1);
                            peak.set(peak.get().max(inflight.get()));
                            let _active = Active(inflight);
                            if pending {
                                let mut yielded = false;
                                poll_fn(move |cx| {
                                    if std::mem::replace(&mut yielded, true) {
                                        Poll::Ready(())
                                    } else {
                                        cx.waker().wake_by_ref();
                                        Poll::Pending
                                    }
                                })
                                .await;
                            }
                            state.change(&context, |value| {
                                *value = json!(value.as_u64().unwrap() + amount);
                                Ok(())
                            })?;
                            Ok(amount)
                        }
                        .boxed_local()
                    })
                    .json_method("portable", |value, _| {
                        async move { Ok(value) }.boxed_local()
                    }))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap();
        let reference = driver
            .run_until(host.get(contract.clone(), json!({}), GetOptions::default()))
            .unwrap();
        let readable = reference.readable("count").unwrap();
        let subscriptions = (0..fanout)
            .map(|_| {
                readable
                    .subscribe(|value| {
                        std::hint::black_box(value);
                        Ok(())
                    })
                    .unwrap()
            })
            .collect();
        Self {
            driver,
            host,
            contract,
            reference,
            active,
            maximum,
            registry: registry_view,
            subscriptions,
        }
    }

    /// Four method calls per turn. All turns are polled concurrently and handlers really suspend.
    pub fn turns(&self, concurrency: usize) -> Vec<u64> {
        self.maximum.set(0);
        let elapsed = self
            .driver
            .run_until(futures::future::join_all((0..concurrency).map(|_| async {
                let start = Instant::now();
                for _ in 0..4 {
                    assert_eq!(
                        self.reference
                            .call::<u64, u64>("run", 1, None)
                            .await
                            .unwrap(),
                        1
                    );
                }
                assert!(self.registry.table(None).unwrap().get("counter").is_some());
                u64::try_from(start.elapsed().as_nanos()).expect("turn duration fits nanoseconds")
            })));
        assert_eq!(self.active.get(), 0);
        assert!(
            self.maximum.get() >= concurrency,
            "workload did not overlap pending handlers"
        );
        elapsed
    }

    pub fn health(&self) -> serde_json::Value {
        #[cfg(feature = "observability-bench")]
        {
            self.host
                .observations()
                .map_or(serde_json::Value::Null, |observations| {
                    serde_json::json!(observations.statistics())
                })
        }
        #[cfg(not(feature = "observability-bench"))]
        {
            serde_json::Value::Null
        }
    }

    pub fn drain(&self) {
        #[cfg(feature = "observability-bench")]
        if let Some(observations) = self.host.observations() {
            let health = observations.statistics();
            assert_eq!(
                health.identifier_exhaustion, 0,
                "recording benchmark exhausted diagnostic identifiers"
            );
            assert_eq!(
                health.dropped_events, 0,
                "recording benchmark overflowed; increase capacity or sampling interval"
            );
            std::hint::black_box(observations.drain(1_048_576));
        }
    }
}
impl Fixture {
    pub fn close_runtime(&mut self) {
        self.subscriptions.clear();
        self.reference.release();
        self.driver.run_until(self.host.close()).unwrap();
    }
    pub fn report(&mut self, case: &str) {
        self.close_runtime();
        self.drain();
        eprintln!(
            "{}",
            serde_json::json!({"case":case,"phase":"after_cleanup","observation_health":self.health()})
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.close_runtime();
        if !std::thread::panicking() {
            self.drain();
        }
    }
}
