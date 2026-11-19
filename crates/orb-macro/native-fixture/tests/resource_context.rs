//! Authored resource methods receive the invocation context and keep transport errors separate.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Host, core::ContextKey};

const TRACE: ContextKey<String> = ContextKey::new("resource/trace", 1);

#[path = "resource_context/lifecycle.rs"]
mod lifecycle;

#[tembra::orb(package = "example:contextual-probe@1.0.0", wit = "wit")]
mod fixed {
    /// A fixed-WIT resource whose host build also checks its component bridge.
    #[wit_resource]
    pub struct Probe;

    impl Probe {
        /// Read the actual context outside the fixed-WIT signature.
        pub fn inspect(&self, prefix: String, context: &tembra::Context) -> tembra::Result<String> {
            Ok(format!("{prefix}:{}", context.principal()?))
        }

        /// Keep transport errors outside the fixed-WIT value.
        pub fn fail(&self) -> tembra::Result<u32> {
            Err(tembra::Error::invalid("probe", "fixed transport error"))
        }
    }

    /// The fixed-WIT provider, without a native service factory.
    #[service("example:contextual/probes@1.0.0")]
    pub struct Probes;

    impl Probes {
        /// Return one provider-owned probe.
        pub fn open() -> Probe {
            Probe
        }
    }
}

#[test]
fn fixed_wit_resource_bridges_preserve_context_and_transport_errors() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![fixed::orb_named("probes", 1).unwrap()])
        .unwrap();
    let reference = Rc::new(
        host.get(
            "probes::example:contextual/probes@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    let client = fixed::ProbesClient::new(reference.clone());
    pool.run_until(host.root("alice", move |context| {
        async move {
            let probe = client.open(&context).await?;
            assert_eq!(
                probe.inspect(&context, "fixed".into()).await?,
                "fixed:alice"
            );
            let failed = probe.fail(&context).await.unwrap_err();
            assert_eq!(
                (failed.code, failed.reason.as_str()),
                (tembra::Code::Invalid, "probe")
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}

#[tembra::orb(package = "example:resource-context@1.0.0")]
mod contextual {
    /// A session that reports its current invocation.
    #[wit_resource]
    pub struct Session;

    impl Session {
        /// Open a session through a fallible synchronous factory.
        pub fn open(context: &tembra::Context) -> tembra::Result<Session> {
            context.check()?;
            Ok(Session)
        }

        /// Refuse asynchronous acquisition without issuing an owner.
        pub async fn try_open() -> tembra::Result<Session> {
            Err(tembra::Error::new(
                tembra::Code::Forbidden,
                "factory",
                "creation refused",
            ))
        }

        /// Read a synchronous value through the transport result alone.
        pub fn number(&self) -> tembra::Result<u32> {
            Ok(7)
        }

        /// Return the domain error separately from a refused transport invocation.
        pub async fn inspect(
            &self,
            prefix: String,
            context: &tembra::Context,
            transport_failure: bool,
        ) -> tembra::Result<std::result::Result<String, String>> {
            if transport_failure {
                return Err(tembra::Error::invalid("session", "transport refused"));
            }
            let address = context.address()?;
            let inspected = format!(
                "{prefix}:{}:{}:{}:{}",
                context.principal()?,
                address.key.join("/"),
                address.principal,
                context.value(&super::TRACE)?.unwrap_or_default(),
            );
            Ok(if prefix == "success" {
                Ok(inspected)
            } else {
                Err(inspected)
            })
        }
    }

    /// The service owning sessions.
    #[service]
    pub struct Sessions;

    impl Sessions {
        /// Identify this interface independently of its resource functions.
        pub fn available() -> bool {
            true
        }
    }
}

#[test]
fn resource_methods_receive_the_invocation_and_keep_domain_errors_inside_transport_results() {
    assert!(contextual::WIT.contains(
        "inspect: async func(prefix: string, transport-failure: bool) -> result<string, string>"
    ));
    assert!(contextual::WIT.contains("open: static func() -> session"));
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![contextual::orb_named("sessions", 1).unwrap()])
        .unwrap();
    let reference = Rc::new(
        host.get(
            "sessions::example:resource-context/sessions@1.0.0",
            vec!["room".into(), "7".into()],
            "alice",
        )
        .unwrap(),
    );
    let client = contextual::SessionsClient::new(reference.clone());
    pool.run_until(reference.ready()).unwrap();
    let claims = host.kernel().diagnostics().references;
    pool.run_until(host.root("alice", move |context| {
        async move {
            let context = context.with(&TRACE, "actual-call".into())?;
            let failed = client.session_try_open(&context).await;
            let Err(failed) = failed else {
                panic!("the factory refuses creation");
            };
            assert_eq!(
                (failed.code, failed.reason.as_str()),
                (tembra::Code::Forbidden, "factory")
            );
            let session = client.session_open(&context).await?;
            assert_eq!(session.number(&context).await?, 7);
            assert_eq!(
                session.inspect(&context, "success".into(), false).await?,
                Ok("success:alice:room/7:alice:actual-call".into())
            );
            assert_eq!(
                session.inspect(&context, "domain".into(), false).await?,
                Err("domain:alice:room/7:alice:actual-call".into())
            );
            let failure = session
                .inspect(&context, "domain".into(), true)
                .await
                .unwrap_err();
            assert_eq!(failure.reason, "session");
            assert_eq!(failure.code, tembra::Code::Invalid);
            assert_eq!(failure.message, "transport refused");
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    pool.run_until(host.settled());
    assert_eq!(host.kernel().diagnostics().references, claims);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}
