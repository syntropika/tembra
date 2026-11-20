//! Canonical clients can refuse a result after execution without retrying provider effects.
use futures::{FutureExt, executor::LocalPool};
use std::{cell::Cell, rc::Rc};
use tembra::{Code, Host};

thread_local! {
    static CALLS: Cell<u32> = const { Cell::new(0) };
}

#[tembra::orb(package = "example:checked-client@1.0.0")]
mod checked {
    pub mod service {
        /// A native caller may validate a canonical string more strictly than the WIT does.
        #[derive(Debug)]
        pub struct Label(String);

        impl TryFrom<String> for Label {
            type Error = &'static str;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.is_empty() {
                    Err("empty label")
                } else {
                    Ok(Self(value))
                }
            }
        }

        /// The same WIT interface as the separately generated implementation below.
        #[service]
        pub struct Labels;

        impl Labels {
            /// Return a label whose native representation validates the string.
            pub fn read(empty: bool) -> Label {
                Label(if empty { String::new() } else { "valid".into() })
            }
        }
    }
}

#[tembra::orb(package = "example:checked-client@1.0.0")]
mod implementation {
    pub mod service {
        /// A provider with exactly the same canonical WIT signature.
        #[service]
        pub struct Labels;

        impl Labels {
            /// Count provider entry before returning a canonical string.
            pub fn read(empty: bool) -> String {
                super::super::CALLS.with(|calls| calls.set(calls.get() + 1));
                if empty { String::new() } else { "valid".into() }
            }
        }
    }
}

#[test]
fn a_client_result_conversion_error_never_retries_an_executed_call() {
    assert_eq!(
        checked::orb().unwrap().declaration().services[0].signature,
        implementation::orb().unwrap().declaration().services[0].signature,
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![implementation::orb().unwrap()]).unwrap();
    let reference = Rc::new(
        host.get(
            "calculator::example:checked-client/labels@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    let client = checked::service::LabelsClient::new(reference.clone());
    let _provider_client = implementation::service::LabelsClient::new(reference.clone());
    CALLS.with(|calls| calls.set(0));
    pool.run_until(host.root("alice", move |context| {
        async move {
            let error = client.read(&context, true).await.unwrap_err();
            assert_eq!(
                (error.code, error.reason.as_str()),
                (Code::Invalid, "result")
            );
            assert!(error.message.contains("empty label"), "{error}");
            CALLS.with(|calls| assert_eq!(calls.get(), 1));
            client.read(&context, false).await?;
            CALLS.with(|calls| assert_eq!(calls.get(), 2));
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
    assert_eq!(host.kernel().diagnostics().calls, 0);
}
