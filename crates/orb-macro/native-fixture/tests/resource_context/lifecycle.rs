//! Fixed-WIT resource methods preserve cancellation and physical ownership.
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{Code, Host, portable::Owned};

#[tembra::orb(package = "example:contextual-host@1.0.0", wit = "wit")]
mod provider {
    use futures::channel::oneshot;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    /// Physical execution controls lent by the host.
    #[inputs]
    pub struct Inputs {
        pub dropped: Rc<Cell<u32>>,
        pub cancelled: Rc<Cell<bool>>,
        pub gate: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
    }

    /// The service implementing the existing fixed contract.
    #[service("example:contextual/sessions@1.0.0")]
    pub struct Sessions {
        dropped: Rc<Cell<u32>>,
        cancelled: Rc<Cell<bool>>,
        gate: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
    }

    impl Sessions {
        /// Bind execution controls to this service object.
        #[create]
        fn create(inputs: &Inputs, _declaration: &tembra::Declaration) -> tembra::Result<Self> {
            Ok(Self {
                dropped: inputs.dropped.clone(),
                cancelled: inputs.cancelled.clone(),
                gate: inputs.gate.clone(),
            })
        }

        /// Open a session owned by this object.
        pub fn open(&self) -> Session {
            Session {
                dropped: self.dropped.clone(),
                cancelled: self.cancelled.clone(),
                gate: self.gate.clone(),
            }
        }
    }

    /// A session whose borrowed receiver stays alive until its method settles.
    #[wit_resource(service = Sessions)]
    pub struct Session {
        dropped: Rc<Cell<u32>>,
        cancelled: Rc<Cell<bool>>,
        gate: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
    }

    impl Session {
        /// Read the actual invocation on a synchronous fixed-WIT method.
        pub fn inspect(&self, prefix: String, context: &tembra::Context) -> tembra::Result<String> {
            Ok(format!(
                "{prefix}:{}:{}",
                context.principal()?,
                context.address()?.key.join("/")
            ))
        }

        /// Observe cancellation and retain the receiver until physical work finishes.
        pub async fn wait(&self, context: &tembra::Context) -> tembra::Result<()> {
            let gate = self.gate.borrow_mut().take().expect("one pending call");
            context.signal().cancelled().await;
            self.cancelled.set(true);
            let _ = gate.await;
            Err(tembra::Error::new(
                tembra::Code::Cancelled,
                "session",
                "physical work ended",
            ))
        }

        /// Consume the owned argument even when this call refuses its operation.
        pub fn refuse(other: Session, context: &tembra::Context) -> tembra::Result<()> {
            context.check()?;
            drop(other);
            Err(tembra::Error::invalid("session", "owned argument refused"))
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }
}

#[test]
fn cancellation_and_release_keep_a_fixed_wit_receiver_until_physical_settlement() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let dropped = Rc::new(Cell::new(0));
    let cancelled = Rc::new(Cell::new(false));
    let (send, gate) = oneshot::channel();
    host.install(vec![
        provider::orb_named(
            "sessions",
            1,
            provider::Inputs {
                dropped: dropped.clone(),
                cancelled: cancelled.clone(),
                gate: Rc::new(RefCell::new(Some(gate))),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(
        host.get(
            "sessions::example:contextual/sessions@1.0.0",
            vec!["room".into()],
            "alice",
        )
        .unwrap(),
    );
    let saved = Rc::new(RefCell::new(None));
    let captured = saved.clone();
    let (release, released) = oneshot::channel();
    let client = provider::SessionsClient::new(reference.clone());
    let result = host
        .spawn_root("alice", move |context| {
            async move {
                let session = client.open(&context).await?;
                assert_eq!(
                    session.inspect(&context, "sync".into()).await?,
                    "sync:alice:room"
                );
                let owned = Owned::from(session);
                let (issued, waiting) = owned
                    .issue_portable::<_, ()>(&context, "[method]session.wait", ())
                    .await?;
                captured.replace(Some(issued));
                let _ = released.await;
                drop(owned);
                waiting.await
            }
            .boxed_local()
        })
        .unwrap();
    pool.run_until_stalled();
    saved.borrow().as_ref().unwrap().cancel();
    release.send(()).unwrap();
    pool.run_until_stalled();
    assert!(cancelled.get());
    assert_eq!(
        dropped.get(),
        0,
        "the physical callback keeps its borrowed receiver"
    );
    send.send(()).unwrap();
    let error = pool.run_until(result).unwrap().unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Cancelled, "session")
    );
    pool.run_until(host.settled());
    assert_eq!(dropped.get(), 1);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}

#[test]
fn failed_owned_submissions_and_retired_resource_calls_release_their_owners() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let dropped = Rc::new(Cell::new(0));
    let orb = || {
        provider::orb_named(
            "sessions",
            1,
            provider::Inputs {
                dropped: dropped.clone(),
                cancelled: Rc::new(Cell::new(false)),
                gate: Rc::new(RefCell::new(None)),
            },
        )
        .unwrap()
    };
    host.install(vec![orb()]).unwrap();
    let reference = Rc::new(
        host.get(
            "sessions::example:contextual/sessions@1.0.0",
            vec!["room".into()],
            "alice",
        )
        .unwrap(),
    );
    let client = provider::SessionsClient::new(reference.clone());
    let session = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let refused = client.open(&context).await?;
                let error = client.session_refuse(&context, refused).await.unwrap_err();
                assert_eq!(error.reason, "session");
                client.open(&context).await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(dropped.get(), 1);
    pool.run_until(host.reload(vec![orb()], 5000)).unwrap();
    assert_eq!(dropped.get(), 2);
    let session = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let error = session.inspect(&context, "stale".into()).await.unwrap_err();
                assert_eq!(
                    (error.code, error.reason.as_str()),
                    (Code::Gone, "resource")
                );
                Ok(session)
            }
            .boxed_local()
        }))
        .unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(
        pool.run_until(host.root("alice", move |context| {
            async move { session.inspect(&context, "closed".into()).await }.boxed_local()
        }))
        .is_err()
    );
    reference.release().unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}
