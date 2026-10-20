use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::core::*;
use tembra::{
    futures::FutureExt,
    serde_json::{self, json},
};

mod generated {
    include!("generated.rs");
}
mod portable {
    include!("portable.rs");
}
mod custom {
    include!("custom.rs");
}
mod forwarded {
    include!("forwarded.rs");
}

struct NativeCounter {
    count: tembra::Readable<generated::variant_local::StateCount>,
    capability: Resource<Cell<u32>>,
}

// Deliberately has no Serde implementation: native dispatch must preserve this value.
pub struct Marker {
    value: Cell<u32>,
}

static CODEC_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static POISON_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct PoisonNative {
    marker: Rc<Marker>,
}

impl tembra::serde::Serialize for PoisonNative {
    fn serialize<S: tembra::serde::Serializer>(
        &self,
        _: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        CODEC_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(tembra::serde::ser::Error::custom(
            "native payload must not serialize",
        ))
    }
}

impl<'de> tembra::serde::Deserialize<'de> for PoisonNative {
    fn deserialize<D: tembra::serde::Deserializer<'de>>(
        _: D,
    ) -> std::result::Result<Self, D::Error> {
        CODEC_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(tembra::serde::de::Error::custom(
            "native payload must not deserialize",
        ))
    }
}

impl generated::CounterProvider for NativeCounter {
    async fn identity(&self, marker: Rc<Marker>, _context: tembra::Context) -> Result<Rc<Marker>> {
        marker.value.set(marker.value.get() + 1);
        Ok(marker)
    }
    async fn poison(&self, value: PoisonNative, _context: tembra::Context) -> Result<PoisonNative> {
        POISON_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(value)
    }
    async fn native_portable(&self, number: u64, _context: tembra::Context) -> Result<u64> {
        Ok(number + 2)
    }
    async fn inspect(&self, context: tembra::Context) -> Result<Context> {
        Ok(context.core().clone())
    }
    fn count(&self) -> tembra::Readable<generated::variant_local::StateCount> {
        self.count.clone()
    }
    fn capability(&self) -> Resource<Cell<u32>> {
        self.capability.clone()
    }
}

struct PortableCounter {
    count: tembra::Cell<portable::variant_default::StateCount>,
}

impl portable::GeneratedContractProvider for PortableCounter {
    async fn increment(
        &self,
        args: portable::v_default_m_increment_args::Root,
        context: tembra::Context,
    ) -> Result<portable::v_default_m_increment_result::Root> {
        self.count.change(&context, |value| {
            *value += args.by;
            Ok(())
        })?;
        self.count.value()
    }
    async fn read(
        &self,
        _context: tembra::Context,
    ) -> Result<portable::v_default_m_read_result::Root> {
        self.count.value()
    }
    async fn touch(&self, _context: tembra::Context) -> Result<()> {
        Ok(())
    }
    fn count(&self) -> tembra::Readable<portable::variant_default::StateCount> {
        self.count.readable()
    }
}

struct UserProvider {
    user: tembra::Cell<custom::variant_main::StateUser>,
}

struct ForwardedCounter {
    count: tembra::Readable<forwarded::variant_local::StateCount>,
}

impl forwarded::GeneratedContractProvider for ForwardedCounter {
    fn count(&self) -> tembra::Readable<forwarded::variant_local::StateCount> {
        self.count.clone()
    }
}

impl custom::GeneratedContractProvider for UserProvider {
    async fn birthday(&self, context: tembra::Context) -> Result<()> {
        self.user.change(&context, |user| {
            user.age += 1;
            user.note = custom::Presence::Present(None);
            Ok(())
        })?;
        Ok(())
    }

    fn user(&self) -> tembra::Readable<custom::variant_main::StateUser> {
        self.user.readable()
    }
}

fn main() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("native-generated", json!("host")),
        driver.clone(),
    );
    let facade = tembra::Host::from_core(host.clone());
    facade
        .serve(generated::Counter, |_, _, declaration| {
            let capability = declaration.core_mut().resource(
                "capability",
                ResourceDeclaration::new(|_| async { Ok(Cell::new(13_u32)) }.boxed_local()),
            )?;
            let count = declaration.state("count", 3_i64)?.readable();
            Ok(NativeCounter { count, capability })
        })
        .unwrap();
    let service = tembra::Host::with_driver("service", "host", driver.clone()).unwrap();
    service
        .serve(portable::GeneratedContract, |input, _, declaration| {
            let initial = if input.name == "forwarded" {
                7_i32
            } else {
                0_i32
            };
            let initialized =
                portable::GeneratedContract::count_state_with_initializer(move |_| async move {
                    let mut first_poll = true;
                    tembra::futures::future::poll_fn(move |context| {
                        if first_poll {
                            first_poll = false;
                            context.waker().wake_by_ref();
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(())
                        }
                    })
                    .await;
                    Ok(initial)
                })?;
            Ok(PortableCounter {
                count: declaration.state_with("count", initialized)?,
            })
        })
        .unwrap();
    service
        .serve(forwarded::GeneratedContract, |_, _, declaration| {
            let source = declaration.get(
                portable::GeneratedContract,
                serde_json::from_value(json!({"name":"forwarded"})).unwrap(),
            )?;
            assert_eq!(source.core().status(), RefStatus::Loading);
            // Capture and derive from the weak pending source before core hydration.
            let count = source.count();
            let derived = declaration.derive((count,), |(value,)| Ok(value))?;
            Ok(ForwardedCounter { count: derived })
        })
        .unwrap();
    let weak_forwarded = driver.run_until(async {
        let reference = service
            .get(
                forwarded::GeneratedContract,
                forwarded::GeneratedContractInput {},
            )
            .await
            .unwrap();
        let readable = reference.count();
        assert_eq!(readable.value().unwrap(), 7);
        drop(reference);
        readable
    });
    driver.run_until_stalled();
    assert!(weak_forwarded.value().is_err());
    let projection_handles = Rc::new(RefCell::new(None));
    let record_projection = projection_handles.clone();
    service
        .serve(custom::GeneratedContract, move |_, _, declaration| {
            let age = declaration.state("projection-source", 99_i64)?;
            let projection =
                custom::GeneratedContract::user_derive(declaration, (age.readable(),), |(age,)| {
                    Ok(custom::variant_main::StateUser {
                        name: "Projected".into(),
                        age,
                        note: if age == 100 {
                            custom::Presence::Present(None)
                        } else {
                            custom::Presence::Missing
                        },
                    })
                })?;
            *record_projection.borrow_mut() = Some((age, projection));
            let initial = custom::variant_main::StateUser {
                name: "Alice".into(),
                age: 99,
                note: custom::Presence::Missing,
            };
            let _previous = declaration.state_with(
                "previous-user",
                custom::GeneratedContract::user_state(initial.clone())?
                    .version(1)
                    .with_migrate::<custom::variant_main::StateUser>(|old, _| Ok(old)),
            )?;
            let initialized = custom::GeneratedContract::user_state_with_initializer(move |_| {
                let initial = initial.clone();
                async move { Ok(initial) }
            })?
            .version(1)
            .with_migrate::<custom::variant_main::StateUser>(|old, _| Ok(old));
            Ok(UserProvider {
                user: declaration.state_with("user", initialized)?,
            })
        })
        .unwrap();
    driver.run_until(async move {
        let raw = facade
            .core()
            .hold(
                generated::contract().unwrap(),
                json!({}),
                Default::default(),
            )
            .unwrap();
        let held = tembra::ContractToken::bind(&generated::Counter, raw).unwrap();
        assert_eq!(held.core().status(), RefStatus::Loading);
        let cancelled = held.clone();
        host.root(move |context| {
            async move {
                let view = cancelled.with_context(&tembra::Context::from_core(context.clone()));
                context.cancel(RuntimeError::new(
                    ErrorCode::Aborted,
                    "generated_cancelled",
                    "cancelled before readiness",
                ));
                let error = view.native_portable(1).await.unwrap_err();
                assert_eq!(error.code, ErrorCode::Aborted);
                assert_eq!(error.reason, "generated_cancelled");
                Ok(())
            }
            .boxed_local()
        })
        .await
        .unwrap();
        assert_eq!(held.native_portable(1).await.unwrap(), 3);
        let input = serde_json::from_value(json!({})).unwrap();
        let counter = facade.get(generated::Counter, input).await.unwrap();
        held.core().release();
        assert!(
            custom::GeneratedContract::user_state(custom::variant_main::StateUser {
                name: "Invalid".into(),
                age: 101,
                note: custom::Presence::Missing,
            })
            .is_err()
        );
        let user = service
            .get(custom::GeneratedContract, custom::GeneratedContractInput {})
            .await
            .unwrap();
        assert_eq!(user.user().value().unwrap().age, 99);
        assert!(matches!(
            user.user().value().unwrap().note,
            custom::Presence::Missing
        ));
        user.birthday().await.unwrap();
        let before = user.user().snapshot().unwrap();
        assert_eq!(before.value().unwrap().age, 100);
        assert!(matches!(
            before.value().unwrap().note,
            custom::Presence::Present(None)
        ));
        assert!(user.birthday().await.is_err());
        let after = user.user().snapshot().unwrap();
        assert_eq!(after.revision(), before.revision());
        assert_eq!(after.value().unwrap().age, 100);
        let (age, projection) = projection_handles.borrow().as_ref().unwrap().clone();
        service
            .root(move |context| async move {
                assert_eq!(projection.value()?.age, 99);
                assert!(matches!(
                    projection.value()?.note,
                    custom::Presence::Missing
                ));
                age.change(&context, |value| {
                    *value = 100;
                    Ok(())
                })?;
                assert_eq!(projection.value()?.age, 100);
                assert!(matches!(
                    projection.value()?.note,
                    custom::Presence::Present(None)
                ));
                let source_before = age.snapshot()?;
                let output_before = projection.snapshot()?;
                assert!(
                    age.change(&context, |value| {
                        *value = 101;
                        Ok(())
                    })
                    .is_err()
                );
                assert_eq!(age.snapshot()?.revision(), source_before.revision());
                assert_eq!(projection.snapshot()?.revision(), output_before.revision());
                assert_eq!(age.value()?, 100);
                assert_eq!(projection.value()?.age, 100);
                Ok::<_, RuntimeError>(())
            })
            .await
            .unwrap();
        let portable_input = serde_json::from_value(json!({"name":"visits"})).unwrap();
        let portable = service
            .get(portable::GeneratedContract, portable_input)
            .await
            .unwrap();
        let args = serde_json::from_value(json!({"by":4})).unwrap();
        assert_eq!(portable.increment(args).await.unwrap(), 4);
        assert_eq!(portable.count().value().unwrap(), 4);
        let float_arguments = portable
            .core()
            .call_json("increment", Some(json!({"by":2.0})), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(float_arguments, json!(6));
        assert_eq!(portable.count().value().unwrap(), 6);
        assert!(
            portable
                .core()
                .call_json("increment", Some(json!({"by":"wrong"})), None)
                .await
                .is_err()
        );
        let pending = service
            .core()
            .hold(
                portable::contract().unwrap(),
                json!({"name":"pending-call"}),
                Default::default(),
            )
            .unwrap();
        let pending = tembra::ContractToken::bind(&portable::GeneratedContract, pending).unwrap();
        assert_eq!(pending.core().status(), RefStatus::Loading);
        assert_eq!(
            pending
                .increment(serde_json::from_value(json!({"by":2})).unwrap())
                .await
                .unwrap(),
            2
        );
        pending.core().release();
        let marker = Rc::new(Marker {
            value: Cell::new(4_u32),
        });
        let returned = counter.identity(marker.clone()).await.unwrap();
        assert!(Rc::ptr_eq(&marker, &returned));
        assert_eq!(marker.value.get(), 5);
        let returned = counter
            .poison(PoisonNative {
                marker: marker.clone(),
            })
            .await
            .unwrap();
        assert!(Rc::ptr_eq(&marker, &returned.marker));
        assert_eq!(POISON_CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
        let returned: PoisonNative = counter
            .core()
            .call(
                "poison",
                PoisonNative {
                    marker: marker.clone(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(Rc::ptr_eq(&marker, &returned.marker));
        assert_eq!(POISON_CALLS.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(CODEC_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(counter.native_portable(4).await.unwrap(), 6);
        assert_eq!(counter.count().value().unwrap(), 3);
        let opaque = counter.capability().unwrap();
        opaque.set(27);
        assert!(Rc::ptr_eq(&opaque, &counter.capability().unwrap()));
        assert_eq!(counter.capability().unwrap().get(), 27);
        let nested = counter.clone();
        let retained = host
            .root_as(json!("caller"), move |context| {
                async move {
                    let view = nested.with_context(&tembra::Context::from_core(context.clone()));
                    let observed = view.inspect().await?;
                    assert_eq!(observed.root_id(), context.root_id());
                    assert_eq!(observed.code_version(), context.code_version());
                    // Child invocation uses the host principal under the same authenticated root.
                    assert_eq!(observed.principal(), &json!("host"));
                    Ok(view)
                }
                .boxed_local()
            })
            .await
            .unwrap();
        assert!(retained.inspect().await.is_err());
        counter.core().release();
        assert!(counter.identity(marker).await.is_err());
        host.close().await.unwrap();
        service.close().await.unwrap();
    });
}
