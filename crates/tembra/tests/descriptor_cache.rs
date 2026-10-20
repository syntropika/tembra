use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Barrier},
};

use tembra::{prelude::*, serde, serde_json::json};

std::thread_local! {
    static SCHEMAS: Cell<usize> = const { Cell::new(0) };
    static SERIALIZES: Cell<usize> = const { Cell::new(0) };
    static RETRIES: Cell<usize> = const { Cell::new(0) };
    static ENTERED: Cell<bool> = const { Cell::new(false) };
    static REENTRIES: Cell<usize> = const { Cell::new(0) };
    static NESTED: RefCell<Option<Rc<tembra::contracts::Contract>>> = const { RefCell::new(None) };
}

#[derive(Deserialize, JsonSchema)]
struct InputFields {
    key: u32,
}

pub struct CountedInput {
    key: u32,
}

impl Serialize for CountedInput {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        SERIALIZES.with(|count| count.set(count.get() + 1));
        use serde::ser::SerializeStruct;
        let mut object = serializer.serialize_struct("CountedInput", 1)?;
        object.serialize_field("key", &self.key)?;
        object.end()
    }
}

impl<'de> Deserialize<'de> for CountedInput {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        InputFields::deserialize(deserializer).map(|value| Self { key: value.key })
    }
}

impl JsonSchema for CountedInput {
    fn schema_name() -> Cow<'static, str> {
        "CountedInput".into()
    }

    fn json_schema(generator: &mut tembra::schemars::SchemaGenerator) -> tembra::schemars::Schema {
        SCHEMAS.with(|count| count.set(count.get() + 1));
        InputFields::json_schema(generator)
    }
}

#[tembra::contract(local, name = "cache.stable", version = 1, input = CountedInput)]
trait Stable {}

#[tembra::contract(local, name = "cache.dependent", version = 1)]
trait Dependent {}

struct Empty;
impl StableProvider for Empty {}
impl DependentProvider for Empty {}

#[tembra::contract(local, name = "cache.teardown", version = 1)]
trait Teardown {}

struct Finalizer;
impl Drop for Finalizer {
    fn drop(&mut self) {
        assert_eq!(Teardown.descriptor().unwrap().name(), "cache.teardown");
    }
}

std::thread_local! {
    static FINALIZER: Finalizer = const { Finalizer };
}

#[test]
fn metadata_remains_available_from_later_tls_destructors() {
    const CHILD: &str = "TEMBRA_DESCRIPTOR_TEARDOWN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "metadata_remains_available_from_later_tls_destructors",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "thread teardown failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    std::thread::spawn(|| {
        // Reverse TLS destruction order makes metadata's cache unavailable to this finalizer.
        FINALIZER.with(|_| {});
        Teardown.descriptor().unwrap();
    })
    .join()
    .unwrap();
}

#[test]
fn successful_metadata_is_shared_without_skipping_acquisition_serializers() {
    std::thread::spawn(|| {
        let first = Stable.descriptor().unwrap();
        let second = Stable.descriptor().unwrap();
        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(SCHEMAS.with(Cell::get), 1);
        let serialized = tembra::serde_json::to_string(&first).unwrap();
        let peer = tembra::contracts::Contract::from_json(&serialized).unwrap();
        assert_eq!(&*first, &peer);
        assert_eq!(first.fingerprint(), peer.fingerprint());

        let driver = tembra::ManualDriver::new();
        let host = Host::with_driver("cache", (), driver.clone()).unwrap();
        host.serve(Stable, |_, _, _| Ok(Empty)).unwrap();
        host.serve(Dependent, |_, _, declaration| {
            declaration.get(Stable, CountedInput { key: 7 })?;
            Ok(Empty)
        })
        .unwrap();
        driver.run_until(async {
            let first = host.get(Stable, CountedInput { key: 7 }).await.unwrap();
            let second = host.get(Stable, CountedInput { key: 7 }).await.unwrap();
            let observed = first.core().contract();
            assert!(Rc::ptr_eq(
                first.core().contract(),
                second.core().contract()
            ));
            assert!(Rc::ptr_eq(observed, &Stable.descriptor().unwrap()));
            host.root(|context| async move {
                context.get(Stable, CountedInput { key: 7 }).await?;
                Ok(())
            })
            .await
            .unwrap();
            host.get(Dependent, DependentInput {}).await.unwrap();
            assert_eq!(SERIALIZES.with(Cell::get), 4);
            assert_eq!(SCHEMAS.with(Cell::get), 1);
            host.close().await.unwrap();
        });
    })
    .join()
    .unwrap();
}

#[test]
fn identical_generated_metadata_is_initialized_independently_on_each_thread() {
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let first = Stable.descriptor().unwrap();
                let second = Stable.descriptor().unwrap();
                assert!(Rc::ptr_eq(&first, &second));
                assert_eq!(SCHEMAS.with(Cell::get), 1);
                let address = Rc::as_ptr(&first) as usize;
                let serialized = tembra::serde_json::to_string(&first).unwrap();
                barrier.wait();
                (address, first.fingerprint().to_owned(), serialized)
            })
        })
        .collect();
    let values: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_ne!(values[0].0, values[1].0);
    assert_eq!(values[0].1, values[1].1);
    assert_eq!(values[0].2, values[1].2);
}

#[derive(Serialize, Deserialize)]
pub struct RetryInput {}

impl JsonSchema for RetryInput {
    fn schema_name() -> Cow<'static, str> {
        "RetryInput".into()
    }

    fn json_schema(_: &mut tembra::schemars::SchemaGenerator) -> tembra::schemars::Schema {
        let attempt = RETRIES.with(|count| {
            count.set(count.get() + 1);
            count.get()
        });
        let schema = if attempt == 1 {
            json!({"type":"object","pattern":"unsupported"})
        } else {
            json!({"type":"object","properties":{},"additionalProperties":false})
        };
        tembra::schemars::Schema::try_from(schema).unwrap()
    }
}

#[tembra::contract(local, name = "cache.retry", version = 1, input = RetryInput)]
trait Retry {}

#[test]
fn failed_construction_retries_without_caching_its_error() {
    std::thread::spawn(|| {
        let error = Retry.descriptor().unwrap_err();
        assert_eq!(error.reason, "unsupported_schema");
        assert_eq!(RETRIES.with(Cell::get), 1);
        let descriptor = Retry.descriptor().unwrap();
        assert_eq!(RETRIES.with(Cell::get), 2);
        assert!(Rc::ptr_eq(&descriptor, &Retry.descriptor().unwrap()));
        assert_eq!(RETRIES.with(Cell::get), 2);
    })
    .join()
    .unwrap();
}

#[derive(Serialize, Deserialize)]
pub struct ReentrantInput {}

impl JsonSchema for ReentrantInput {
    fn schema_name() -> Cow<'static, str> {
        "ReentrantInput".into()
    }

    fn json_schema(_: &mut tembra::schemars::SchemaGenerator) -> tembra::schemars::Schema {
        REENTRIES.with(|count| count.set(count.get() + 1));
        let nested = ENTERED.with(|entered| entered.replace(true));
        if !nested {
            NESTED.with(|value| *value.borrow_mut() = Some(Reentrant.descriptor().unwrap()));
            ENTERED.with(|entered| entered.set(false));
        }
        tembra::schemars::Schema::try_from(json!({
            "type":"object","properties":{},"additionalProperties":false,
            "description":if nested {"first successful schema"} else {"outer candidate"}
        }))
        .unwrap()
    }
}

#[tembra::contract(local, name = "cache.reentrant", version = 1, input = ReentrantInput)]
trait Reentrant {}

#[test]
fn callbacks_run_outside_cache_initialization_and_first_success_wins() {
    std::thread::spawn(|| {
        let descriptor = Reentrant.descriptor().unwrap();
        NESTED.with(|nested| assert!(Rc::ptr_eq(&descriptor, nested.borrow().as_ref().unwrap())));
        assert_eq!(descriptor.input()["description"], "first successful schema");
        assert_eq!(REENTRIES.with(Cell::get), 2);
        assert!(Rc::ptr_eq(&descriptor, &Reentrant.descriptor().unwrap()));
        assert_eq!(REENTRIES.with(Cell::get), 2);
    })
    .join()
    .unwrap();
}

struct Manual(Cell<usize>);

impl ContractToken for Manual {
    type Input = ();
    type Reference = tembra::core::Reference;

    fn descriptor(&self) -> Result<Rc<tembra::contracts::Contract>> {
        self.0.set(self.0.get() + 1);
        tembra::contracts::Contract::local(
            "cache.manual",
            1,
            json!({"type":"object","properties":{},"description":self.0.get().to_string()}),
            BTreeMap::new(),
        )
        .map(Rc::new)
        .map_err(|error| RuntimeError::invalid(error.reason, error.message))
    }

    fn bind(&self, reference: Self::Reference) -> Result<Self::Reference> {
        Ok(reference)
    }
}

#[test]
fn manual_tokens_keep_their_explicit_construction_policy() {
    let token = Manual(Cell::new(0));
    let first = token.descriptor().unwrap();
    let second = token.descriptor().unwrap();
    assert_eq!(token.0.get(), 2);
    assert!(!Rc::ptr_eq(&first, &second));
    assert_ne!(first.fingerprint(), second.fingerprint());
}

const DESCRIPTOR: usize = 4;
const __TEMBRA_DESCRIPTOR_CACHE0: usize = 5;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ConstInput<const N: usize> {}

#[tembra::contract(
    local, name = "cache.hygiene", version = 1,
    input = ConstInput<{ __TEMBRA_DESCRIPTOR_CACHE0 }>,
)]
trait Hygiene {
    async fn echo(&self, value: [u8; DESCRIPTOR]) -> Result<[u8; DESCRIPTOR]>;
}

#[test]
fn private_cache_names_do_not_capture_constants_in_inputs_or_native_signatures() {
    let descriptor = Hygiene.descriptor().unwrap();
    assert_eq!(descriptor.name(), "cache.hygiene");
    let members = descriptor.members("").unwrap();
    let tembra::contracts::Member::Method { args, result } = &members["echo"] else {
        panic!("echo must remain a method");
    };
    assert_eq!(args, result);
    assert!(
        matches!(args, Some(tembra::contracts::ValueSchema::Native(id)) if id.contains("[u8;4]"))
    );
}
