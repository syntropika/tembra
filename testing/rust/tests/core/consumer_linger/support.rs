use super::*;

pub(super) const NAME: &str = "linger-counter";
pub(super) fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}
pub(super) fn policy(ttl: u64, cap: usize) -> ConsumerLingerOptions {
    ConsumerLingerOptions::new(BTreeMap::from([(NAME.into(), ms(ttl))]), cap).unwrap()
}

type ReleaseHook = Rc<RefCell<Option<Box<dyn Fn()>>>>;

struct Counted {
    inner: Rc<dyn LinkTx>,
    frames: Rc<RefCell<Vec<Value>>>,
    on_release: Option<ReleaseHook>,
}
impl LinkTx for Counted {
    fn try_send(&self, bytes: Rc<[u8]>) -> std::result::Result<(), SendError> {
        self.inner.try_send(bytes.clone())?;
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        let release = frame["t"] == "release";
        self.frames.borrow_mut().push(frame);
        if release && let Some(hook) = &self.on_release {
            let callback = hook.borrow_mut().take();
            if let Some(callback) = callback {
                callback();
            }
        }
        Ok(())
    }
    fn close(&self, reason: CloseReason) {
        self.inner.close(reason);
    }
}
pub(super) fn counted(link: Link, frames: &Rc<RefCell<Vec<Value>>>) -> Link {
    counted_with_hook(link, frames, None)
}
fn counted_with_hook(
    link: Link,
    frames: &Rc<RefCell<Vec<Value>>>,
    on_release: Option<ReleaseHook>,
) -> Link {
    Link {
        tx: Rc::new(Counted {
            inner: link.tx,
            frames: frames.clone(),
            on_release,
        }),
        events: link.events,
    }
}

pub(super) struct Fixture {
    pub(super) driver: Rc<ManualDriver>,
    pub(super) client: Host,
    pub(super) server: Host,
    pub(super) token: Rc<Contract>,
    pub(super) sent: Rc<RefCell<Vec<Value>>>,
    pub(super) received: Rc<RefCell<Vec<Value>>>,
    pub(super) cells: Rc<RefCell<BTreeMap<String, Cell>>>,
    pub(super) closed: Rc<RefCell<Vec<String>>>,
    pub(super) outbound: Connection,
    pub(super) inbound: Connection,
    pub(super) wait: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
    pub(super) started: Rc<Counter<bool>>,
    pub(super) on_release: ReleaseHook,
}
impl Fixture {
    pub(super) fn new(options: ConsumerLingerOptions, quota: usize, provider_ttl: u64) -> Self {
        Self::with_connection_limit(options, quota, provider_ttl, RpcLimits::default().max_holds)
    }
    pub(super) fn with_connection_limit(
        options: ConsumerLingerOptions,
        quota: usize,
        provider_ttl: u64,
        connection_limit: usize,
    ) -> Self {
        Self::configured(options, quota, provider_ttl, connection_limit, true)
    }
    pub(super) fn resolver_connection_limit(
        options: ConsumerLingerOptions,
        quota: usize,
        provider_ttl: u64,
        connection_limit: usize,
    ) -> Self {
        Self::configured(options, quota, provider_ttl, connection_limit, false)
    }
    fn configured(
        options: ConsumerLingerOptions,
        quota: usize,
        provider_ttl: u64,
        connection_limit: usize,
        static_route: bool,
    ) -> Self {
        let driver = ManualDriver::new();
        let limits = HostRpcLimits {
            max_remote_holds: quota,
            ..HostRpcLimits::default()
        };
        let client = Host::new(
            HostOptions::new("client", json!("ui"))
                .with_rpc_limits(limits)
                .unwrap()
                .with_consumer_linger(options),
            driver.clone(),
        );
        let server = Host::new(HostOptions::new("server", json!("service")), driver.clone());
        server
            .set_rpc_identity(
                RootOrigin::new("server", "11111111111111111111111111111111").unwrap(),
            )
            .unwrap();
        let token = Rc::new(
            Contract::remote(
                NAME,
                1,
                json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}),
                Placement::Named {
                    name: "server".into(),
                },
                BTreeMap::from([(
                    "default".into(),
                    BTreeMap::from([
                        (
                            "count".into(),
                            Member::State {
                                schema: json!({"type":"integer"}),
                            },
                        ),
                        (
                            "noop".into(),
                            Member::Method {
                                args: None,
                                result: None,
                            },
                        ),
                        (
                            "wait".into(),
                            Member::Method {
                                args: None,
                                result: None,
                            },
                        ),
                    ]),
                )]),
            )
            .unwrap(),
        );
        let cells = Rc::new(RefCell::new(BTreeMap::new()));
        let closed = Rc::new(RefCell::new(Vec::new()));
        let wait = Rc::new(RefCell::new(None::<oneshot::Receiver<()>>));
        let started = Rc::new(Counter::new(false));
        let (save, close, waiting, entered) =
            (cells.clone(), closed.clone(), wait.clone(), started.clone());
        server
            .serve(
                token.clone(),
                Rc::new(move |input, _, declaration| {
                    let id = input["id"].as_str().unwrap().to_owned();
                    let cell = declaration.state(
                        "count",
                        StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                    )?;
                    save.borrow_mut().insert(id.clone(), cell.clone());
                    let close = close.clone();
                    let resource = declaration.resource(
                        "lifetime",
                        ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                            move |_| {
                                close.borrow_mut().push(id.clone());
                                async { Ok(()) }.boxed_local()
                            },
                        ),
                    )?;
                    let (waiting, entered) = (waiting.clone(), entered.clone());
                    Ok(Implementation::new()
                        .variant("default")
                        .state("count", cell.readable())
                        .json_method("noop", |_, _| async { Ok(None) }.boxed_local())
                        .json_method("wait", move |_, _| {
                            let receive = waiting.borrow_mut().take().unwrap();
                            let resource = resource.clone();
                            entered.set(true);
                            async move {
                                receive.await.unwrap();
                                resource.value()?;
                                Ok(None)
                            }
                            .boxed_local()
                        }))
                }),
                ServeOptions {
                    linger: ms(provider_ttl),
                    ..ServeOptions::default()
                },
            )
            .unwrap();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let received = Rc::new(RefCell::new(Vec::new()));
        let (left, right, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let on_release: ReleaseHook = Rc::new(RefCell::new(None));
        let mut connection_options = ConnectionOptions::default();
        connection_options.limits.max_holds = connection_limit;
        let outbound = client
            .connect_client(
                counted_with_hook(left, &sent, Some(on_release.clone())),
                "server",
                connection_options,
            )
            .unwrap();
        let inbound = server
            .accept_client(
                counted(right, &received),
                json!("accepted-user"),
                ConnectionOptions::default(),
            )
            .unwrap();
        if static_route {
            client.set_route("server", outbound.clone()).unwrap();
        } else {
            let route = outbound.clone();
            client
                .set_resolver(Rc::new(move |_| {
                    let route = route.clone();
                    async move { Ok(route) }.boxed_local()
                }))
                .unwrap();
        }
        driver.run_until_stalled();
        Self {
            driver,
            client,
            server,
            token,
            sent,
            received,
            cells,
            closed,
            outbound,
            inbound,
            wait,
            started,
            on_release,
        }
    }
    pub(super) fn get(&self, id: &str, state: bool) -> Reference {
        self.driver
            .run_until(self.client.get(
                self.token.clone(),
                json!({"id":id}),
                GetOptions {
                    state,
                    ..GetOptions::default()
                },
            ))
            .unwrap()
    }
    pub(super) fn count(&self, tag: &str) -> usize {
        self.sent
            .borrow()
            .iter()
            .filter(|frame| frame["t"] == tag)
            .count()
    }
    pub(super) fn incoming(&self, tag: &str) -> usize {
        self.received
            .borrow()
            .iter()
            .filter(|frame| frame["t"] == tag)
            .count()
    }
    pub(super) fn advance(&self, millis: u64) {
        self.driver.advance(ms(millis));
        self.driver.run_until_stalled();
    }
    pub(super) fn write(&self, id: &str, value: i64) {
        let cell = self.cells.borrow()[id].clone();
        self.driver
            .run_until(self.server.root(move |ctx| {
                async move { cell.replace(&ctx, json!(value), Default::default()) }.boxed_local()
            }))
            .unwrap();
        self.driver.run_until_stalled();
    }
    pub(super) fn close(&self) {
        self.driver.run_until(self.client.close()).unwrap();
        self.driver.run_until(self.server.close()).unwrap();
        self.driver.run_until_stalled();
    }
}

pub(super) fn forwarding_parent(f: &Fixture) -> (Host, Rc<dyn LinkTx>, Connection) {
    f.client
        .set_rpc_identity(RootOrigin::new("client", "22222222222222222222222222222222").unwrap())
        .unwrap();
    f.client.set_route_authorizer(Rc::new(|_| Ok(()))).unwrap();
    // The middle Host may forward only over its authenticated child edge.
    let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
    let child = f
        .client
        .accept_child(
            counted(left, &f.sent),
            "server",
            json!("child"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let _server_parent = f
        .server
        .connect_parent(
            right,
            "client",
            json!("parent"),
            ConnectionOptions::default(),
        )
        .unwrap();
    f.client.set_route("server", child).unwrap();
    let parent = Host::new(
        HostOptions::new("parent", json!("parent-self")),
        f.driver.clone(),
    );
    parent
        .set_rpc_identity(RootOrigin::new("parent", "33333333333333333333333333333333").unwrap())
        .unwrap();
    let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
    let authority = left.tx.clone();
    let to_middle = parent
        .accept_child(left, "client", json!("child"), ConnectionOptions::default())
        .unwrap();
    let source = f
        .client
        .connect_parent(
            right,
            "parent",
            json!("alice"),
            ConnectionOptions::default(),
        )
        .unwrap();
    parent.set_route("server", to_middle.clone()).unwrap();
    (parent, authority, source)
}
