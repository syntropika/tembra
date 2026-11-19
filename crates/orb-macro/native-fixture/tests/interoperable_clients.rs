//! One generated client calls separately generated implementations of a fixed WIT contract.
use futures::{FutureExt, executor::LocalPool};
use std::{any::TypeId, cell::RefCell, rc::Rc};
use tembra::{Code, Host};

thread_local! {
    static CALLS: RefCell<Vec<(&'static str, &'static str)>> = const { RefCell::new(Vec::new()) };
}

/// Record provider entry, independently of codec and transport processing.
fn entered(provider: &'static str, method: &'static str) {
    CALLS.with(|calls| calls.borrow_mut().push((provider, method)));
}

// Each expansion has its own bindings, canonical codecs and Rust nominal types.
macro_rules! implementation {
    ($module:ident) => {
        #[tembra::orb(package = "example:client-fixture@1.0.0", wit = "wit")]
        mod $module {
            use std::rc::Rc;

            /// The shared WIT interface with an additional native-only method.
            #[service("example:interoperable/values@1.0.0")]
            pub struct Values;

            impl Values {
                /// Return independently generated record types through the canonical codec.
                pub fn echo(value: values::Item) -> values::Item {
                    super::entered(stringify!($module), "echo");
                    value
                }

                /// Preserve domain success and failure inside the transport result.
                pub async fn choose(value: values::Choice) -> Result<values::Item, String> {
                    super::entered(stringify!($module), "choose");
                    match value {
                        values::Choice::Accept(item) => Ok(item),
                        values::Choice::Reject(reason) => Err(reason),
                    }
                }

                /// Return an outer transport error without re-executing the provider.
                pub fn fail() -> tembra::Result<u32> {
                    super::entered(stringify!($module), "fail");
                    Err(tembra::Error::new(
                        tembra::Code::Forbidden,
                        "fixture",
                        "refused",
                    ))
                }

                /// Exercise conversion refusals before and after provider entry.
                pub fn floating(value: f64) -> f64 {
                    super::entered(stringify!($module), "floating");
                    if value == 0.0 { f64::NAN } else { value }
                }

                /// Consume the original owner at its issuing provider.
                pub fn consume(value: Cursor) -> values::Item {
                    super::entered(stringify!($module), "consume");
                    value.0
                }

                /// Keep native-only values on the direct Rust entrypoint.
                #[local]
                pub fn apply(value: u32, operation: Rc<dyn Fn(u32) -> u32>) -> u32 {
                    super::entered(stringify!($module), "apply");
                    operation(value)
                }
            }

            /// A resource whose client wrapper is independent of its provider's Rust type.
            #[wit_resource(service = Values)]
            pub struct Cursor(values::Item);

            impl Cursor {
                /// Issue one owner with the shared nominal WIT identity.
                #[constructor]
                pub fn new(value: values::Item) -> Self {
                    super::entered(stringify!($module), "new");
                    Self(value)
                }

                /// Read through the portable receiver loan.
                pub async fn read(&self) -> values::Item {
                    super::entered(stringify!($module), "read");
                    self.0.clone()
                }
            }
        }
    };
}

implementation!(first);
implementation!(second);

/// The client selects neither a native dispatch mode nor its provider's generated Rust types.
#[test]
fn one_client_interoperates_with_independently_generated_implementations() {
    assert_ne!(
        TypeId::of::<first::values::Item>(),
        TypeId::of::<second::values::Item>()
    );
    assert_ne!(
        TypeId::of::<first::values::Choice>(),
        TypeId::of::<second::values::Choice>()
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        first::orb_named("first", 1).unwrap(),
        second::orb_named("second", 1).unwrap(),
    ])
    .unwrap();
    for provider in ["first", "second"] {
        let reference = Rc::new(
            host.get(
                &format!("{provider}::example:interoperable/values@1.0.0"),
                vec!["main".into()],
                "alice",
            )
            .unwrap(),
        );
        let client = first::ValuesClient::new(reference.clone());
        let _independent_client = second::ValuesClient::new(reference.clone());
        CALLS.with(|calls| calls.borrow_mut().clear());
        pool.run_until(host.root("alice", move |context| {
            async move {
                let item = || first::values::Item {
                    label: "record".into(),
                    amount: u64::MAX,
                };
                let echoed = client.echo(&context, item()).await?;
                assert_eq!((echoed.label.as_str(), echoed.amount), ("record", u64::MAX));
                let chosen = client
                    .choose(&context, first::values::Choice::Accept(item()))
                    .await?
                    .unwrap();
                assert_eq!(chosen.amount, u64::MAX);
                assert_eq!(
                    client
                        .choose(&context, first::values::Choice::Reject("domain".into()))
                        .await?
                        .unwrap_err(),
                    "domain"
                );
                let failed = client.fail(&context).await.unwrap_err();
                assert_eq!(
                    (failed.code, failed.reason.as_str(), failed.message.as_str()),
                    (Code::Forbidden, "fixture", "refused")
                );
                assert_eq!(
                    client
                        .floating(&context, f64::NAN)
                        .await
                        .unwrap_err()
                        .reason,
                    "arguments"
                );
                assert_eq!(
                    client.floating(&context, 0.0).await.unwrap_err().reason,
                    "result"
                );
                assert_eq!(client.floating(&context, 2.0).await?, 2.0);
                assert_eq!(
                    client
                        .apply(&context, 5, Rc::new(|value| value + 1))
                        .await?,
                    6
                );
                let cursor = client.new_cursor(&context, item()).await?;
                assert_eq!(cursor.read(&context).await?.amount, u64::MAX);
                assert_eq!(client.consume(&context, cursor).await?.amount, u64::MAX);
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
        CALLS.with(|calls| {
            assert_eq!(
                *calls.borrow(),
                [
                    (provider, "echo"),
                    (provider, "choose"),
                    (provider, "choose"),
                    (provider, "fail"),
                    (provider, "floating"),
                    (provider, "floating"),
                    (provider, "apply"),
                    (provider, "new"),
                    (provider, "read"),
                    (provider, "consume"),
                ]
            )
        });
        reference.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
    assert_eq!(host.kernel().diagnostics().calls, 0);
}
