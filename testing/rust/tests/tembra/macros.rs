use std::{cell::Cell as Flag, rc::Rc};

use tembra::{
    Cell, Context, ContractToken, Host, ProviderContract, Readable, Result,
    core::{ManualDriver, Resource, ResourceDeclaration},
};

#[tembra::contract(local, name = "typed.counter", version = 1)]
pub trait Counter {
    #[state]
    fn count(&self) -> u64;
    async fn increment(&self, by: u64) -> Result<u64>;
    async fn sum(&self, left: u32, right: u32) -> Result<u32>;
    async fn reset(&self) -> Result<()>;
}

struct CounterService {
    count: Cell<u64>,
}

impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u64> {
        self.count.readable()
    }
    async fn increment(&self, by: u64, ctx: Context) -> Result<u64> {
        self.count.change(&ctx, |value| {
            *value += by;
            Ok(())
        })?;
        self.count.value()
    }
    async fn sum(&self, left: u32, right: u32, _ctx: Context) -> Result<u32> {
        Ok(left + right)
    }
    async fn reset(&self, ctx: Context) -> Result<()> {
        self.count.change(&ctx, |value| {
            *value = 0;
            Ok(())
        })?;
        Ok(())
    }
}

#[test]
fn native_provider_and_reference_keep_core_owned_state_and_dispatch() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::with_driver("typed-counter", (), driver.clone())?;
    (|| -> tembra::Result<()> {
        let provider_contract = Counter;
        host.install(
            tembra::NativeFacet::new("counter-provider", vec![], move |_, setup| {
                let count_definition = setup.state("count")?;
                setup.serve(provider_contract, move |_, _, declaration| {
                    Ok(CounterService {
                        count: declaration.state(count_definition.value(0_u64)?)?,
                    })
                })
            })?,
            Default::default(),
        )
        .map(drop)
    })()?;
    driver.run_until(async {
        let counter = host.get(Counter, CounterInput {}).await?;
        let Err(mismatch) = PortableLocalRef::from_reference(counter.core().clone()) else {
            panic!("a different consumer contract was accepted");
        };
        assert_eq!(mismatch.code, tembra::ErrorCode::ContractMismatch);
        assert_eq!(mismatch.reason, "consumer_contract");
        assert_eq!(counter.increment(2).await?, 2);
        assert_eq!(counter.count().value()?, 2);
        assert_eq!(counter.sum(3, 4).await?, 7);
        counter.reset().await?;
        assert_eq!(counter.count().value()?, 0);
        host.close().await
    })
}

struct NativeValue {
    flag: Flag<bool>,
    calls: Flag<usize>,
}

std::thread_local! {
    static NATIVE_SERIALIZES: Flag<usize> = const { Flag::new(0) };
    static NATIVE_DESERIALIZES: Flag<usize> = const { Flag::new(0) };
}

impl tembra::serde::Serialize for NativeValue {
    fn serialize<S: tembra::serde::Serializer>(
        &self,
        _serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        NATIVE_SERIALIZES.with(|count| count.set(count.get() + 1));
        panic!("native dispatch must not serialize the sentinel")
    }
}

impl<'de> tembra::serde::Deserialize<'de> for NativeValue {
    fn deserialize<D: tembra::serde::Deserializer<'de>>(
        _deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        NATIVE_DESERIALIZES.with(|count| count.set(count.get() + 1));
        panic!("native dispatch must not deserialize the sentinel")
    }
}

#[tembra::contract(local, name = "typed.native-identity", version = 1)]
trait Identity {
    #[opaque(id = "typed.value@1")]
    fn value(&self) -> NativeValue;
    async fn echo(&self, value: Rc<NativeValue>) -> Result<Rc<NativeValue>>;
}

struct IdentityService {
    value: Resource<NativeValue>,
}

impl IdentityProvider for IdentityService {
    fn value(&self) -> Resource<NativeValue> {
        self.value.clone()
    }
    async fn echo(&self, value: Rc<NativeValue>, _ctx: Context) -> Result<Rc<NativeValue>> {
        value.flag.set(true);
        value.calls.set(value.calls.get() + 1);
        Ok(value)
    }
}

#[test]
fn native_method_sentinels_and_opaque_resources_preserve_rc_identity() -> Result<()> {
    NATIVE_SERIALIZES.with(|count| count.set(0));
    NATIVE_DESERIALIZES.with(|count| count.set(0));
    let driver = ManualDriver::new();
    let host = Host::with_driver("typed-native", (), driver.clone())?;
    host.serve(Identity, |_, _, declaration| {
        let value = declaration.core_mut().resource(
            "native",
            ResourceDeclaration::new(|_| {
                Box::pin(async {
                    Ok(NativeValue {
                        flag: Flag::new(false),
                        calls: Flag::new(0),
                    })
                })
            }),
        )?;
        Ok(IdentityService { value })
    })?;
    driver.run_until(async {
        let identity = host.get(Identity, IdentityInput {}).await?;
        let value = identity.value()?;
        assert!(Rc::ptr_eq(&value, &identity.value()?));
        let result = identity.echo(value.clone()).await?;
        assert!(Rc::ptr_eq(&value, &result));
        assert!(value.flag.get());
        assert_eq!(value.calls.get(), 1);
        let direct: Rc<NativeValue> = identity.core().call("echo", value.clone(), None).await?;
        assert!(Rc::ptr_eq(&value, &direct));
        assert_eq!(value.calls.get(), 2);
        NATIVE_SERIALIZES.with(|count| assert_eq!(count.get(), 0));
        NATIVE_DESERIALIZES.with(|count| assert_eq!(count.get(), 0));
        host.close().await
    })
}

#[tembra::contract(remote, name = "typed.remote", version = 1, host = "typed-portable")]
pub trait RemoteCounter {
    async fn add(&self, left: u64, right: u64) -> Result<u64>;
    async fn ping(&self) -> Result<()>;
    async fn read(&self) -> Result<u64>;
    async fn echo(&self, value: Option<u64>) -> Result<Option<u64>>;
}

struct RemoteService(Rc<Flag<u32>>);

impl RemoteCounterProvider for RemoteService {
    async fn add(&self, left: u64, right: u64, _ctx: Context) -> Result<u64> {
        self.0.set(self.0.get() + 1);
        Ok(left + right)
    }
    async fn ping(&self, _ctx: Context) -> Result<()> {
        Ok(())
    }
    async fn read(&self, _ctx: Context) -> Result<u64> {
        Ok(7)
    }
    async fn echo(&self, value: Option<u64>, _ctx: Context) -> Result<Option<u64>> {
        Ok(value)
    }
}

#[test]
fn remote_methods_validate_portable_values_in_the_same_host() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::with_driver("typed-portable", (), driver.clone())?;
    let entered = Rc::new(Flag::new(0));
    let effects = entered.clone();
    host.serve(RemoteCounter, move |_, _, _| {
        Ok(RemoteService(effects.clone()))
    })?;
    driver.run_until(async {
        let counter = host.get(RemoteCounter, RemoteCounterInput {}).await?;
        assert_eq!(counter.add(2, 3).await?, 5);
        assert_eq!(counter.read().await?, 7);
        counter.ping().await?;
        assert_eq!(counter.echo(None).await?, None);
        assert_eq!(counter.echo(Some(3)).await?, Some(3));
        assert_eq!(
            counter.add(u64::MAX, 1).await.unwrap_err().reason,
            "argument_conversion"
        );
        let before = entered.get();
        let error = counter
            .core()
            .call_json(
                "add",
                Some(tembra::serde_json::json!({
                    "left": -1, "right": 0
                })),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason, "args");
        assert_eq!(entered.get(), before);
        host.close().await
    })
}

#[tembra::contract(local, name = "typed.portable-local", version = 1)]
trait PortableLocal {
    #[portable]
    async fn echo(&self, value: String) -> Result<String>;
}

struct PortableService;
impl PortableLocalProvider for PortableService {
    async fn echo(&self, value: String, _ctx: Context) -> Result<String> {
        Ok(value)
    }
}

#[test]
fn explicit_portable_local_channels_are_available_to_foreign_bindings() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::with_driver("typed-portable-local", (), driver.clone())?;
    host.serve(PortableLocal, |_, _, _| Ok(PortableService))?;
    driver.run_until(async {
        let reference = host.get(PortableLocal, PortableLocalInput {}).await?;
        assert_eq!(reference.echo("hello".into()).await?, "hello");
        let raw = reference.core();
        assert_eq!(
            raw.call_json("echo", Some("world".into()), None).await?,
            Some("world".into())
        );
        host.close().await
    })
}

#[test]
fn native_and_portable_provider_channels_follow_descriptor_kind() -> Result<()> {
    let local = Counter.descriptor()?;
    assert!(matches!(
        local.members("").unwrap()["increment"],
        tembra::contracts::Member::Method {
            args: Some(tembra::contracts::ValueSchema::Native(_)),
            ..
        }
    ));
    let portable = RemoteCounter.descriptor()?;
    assert!(matches!(
        portable.members("main").unwrap()["add"],
        tembra::contracts::Member::Method {
            args: Some(tembra::contracts::ValueSchema::Portable(_)),
            ..
        }
    ));
    // Registration is a synchronous conversion, with no extra factory or scheduler.
    let _implementation = RemoteCounter.implementation(RemoteService(Rc::new(Flag::new(0))))?;
    Ok(())
}

#[derive(Clone, Copy)]
struct ArgumentReceiver;

impl ArgumentReceiver {
    async fn echo(&self, _value: Self, _ctx: Context) -> Result<u32> {
        Ok(999)
    }
}

#[tembra::contract(local, name = "typed.hygienic-receiver", version = 1)]
trait Hygienic {
    async fn echo(&self, __tembra_provider: ArgumentReceiver) -> Result<u32>;
    #[portable]
    async fn portable_echo(&self, __tembra_provider: u32, __tembra_capture: u32) -> Result<u32>;
    async fn raw(&self, args: u32) -> Result<u32>;
    async fn context(&self, args: u32) -> Result<u32>;
}

struct HygienicService(Rc<Flag<bool>>);
impl HygienicProvider for HygienicService {
    async fn echo(&self, _argument: ArgumentReceiver, _ctx: Context) -> Result<u32> {
        self.0.set(true);
        Ok(7)
    }
    async fn raw(&self, args: u32, _ctx: Context) -> Result<u32> {
        Ok(args)
    }
    async fn portable_echo(&self, left: u32, right: u32, _ctx: Context) -> Result<u32> {
        Ok(left + right)
    }
    async fn context(&self, args: u32, _ctx: Context) -> Result<u32> {
        Ok(args)
    }
}

#[test]
fn user_argument_bindings_cannot_replace_the_managed_provider_receiver() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::with_driver("hygienic", (), driver.clone())?;
    let called = Rc::new(Flag::new(false));
    let observed = called.clone();
    host.serve(Hygienic, move |_, _, _| {
        Ok(HygienicService(observed.clone()))
    })?;
    driver.run_until(async {
        assert_eq!(
            host.root(|ctx| async move { ArgumentReceiver.echo(ArgumentReceiver, ctx).await })
                .await?,
            999
        );
        let reference = host.get(Hygienic, HygienicInput {}).await?;
        assert_eq!(reference.echo(ArgumentReceiver).await?, 7);
        assert!(called.get());
        assert_eq!(reference.portable_echo(2, 4).await?, 6);
        assert_eq!(reference.raw(3).await?, 3);
        assert_eq!(reference.context(5).await?, 5);
        host.close().await
    })
}
