//! Build native state and resources during synchronous declaration.

use super::*;

pub struct Declaration {
    pub(super) host: Host,
    pub(super) object: core::Id,
    pub(super) resources: Rc<RefCell<Vec<core::Id>>>,
}

impl Declaration {
    /// Read the native object's canonical service address.
    pub fn address(&self) -> Result<core::Address> {
        self.host.kernel().object_address(self.object)
    }

    /// Declare one typed object state owned by this scope.
    pub fn state<T: Serialize + 'static>(
        &self,
        initial: T,
        version: u32,
        validate: impl Fn(&T) -> Result<()> + 'static,
    ) -> Result<State<T>> {
        State::new(self.host.clone(), self.object, initial, version, validate)
    }

    /// Declares a synchronous opener. The kernel runs it after the factory returns.
    pub fn resource<T: 'static>(
        &self,
        name: &str,
        version: u32,
        open: impl FnOnce() -> Result<T> + 'static,
        close: impl FnOnce(Rc<T>, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<Resource<T>> {
        self.resource_async(
            name,
            version,
            &[],
            move |_| async move { open() }.boxed_local(),
            close,
        )
    }

    /// Declares an asynchronous resource whose dependencies must physically open first.
    pub fn resource_async<T: 'static>(
        &self,
        name: &str,
        version: u32,
        dependencies: &[ResourceKey],
        open: impl FnOnce(ResourceSignal) -> LocalBoxFuture<'static, Result<T>> + 'static,
        close: impl FnOnce(Rc<T>, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<Resource<T>> {
        if dependencies
            .iter()
            .any(|dependency| !self.host.same(&dependency.host))
        {
            return Err(Error::invalid(
                Reason::ResourceHost,
                "resource dependencies belong to another host",
            ));
        }
        let record = self.host.kernel().resource(
            core::Owner::Object(self.object),
            name.into(),
            version,
            false,
            dependencies
                .iter()
                .map(|dependency| dependency.id)
                .collect(),
        )?;
        self.resources.borrow_mut().push(record.id);
        let host = self.host.clone();
        let resource = record.id;
        let (send, receive) = oneshot::channel();
        let signal = ResourceSignal {
            cancelled: Rc::new(Cell::new(false)),
            wait: async move {
                let _ = receive.await;
            }
            .boxed_local()
            .shared(),
        };
        self.host
            .0
            .cancellations
            .borrow_mut()
            .insert(resource, (signal.clone(), send));
        self.host.0.openers.borrow_mut().insert(
            resource,
            Box::new(move || {
                async move {
                    signal.check()?;
                    let value = Rc::new(open(signal.clone()).await?);
                    let id = host.kernel().allocate_handle()?;
                    host.0.values.borrow_mut().insert(
                        id,
                        ValueRecord {
                            value: value.clone(),
                            portable: None,
                        },
                    );
                    host.0
                        .closers
                        .borrow_mut()
                        .insert(resource, Box::new(move |context| close(value, context)));
                    if let Err(error) = host.kernel().resource_opened(resource, id) {
                        host.drop_value(id);
                        return Err(error);
                    }
                    signal.check()
                }
                .boxed_local()
            }),
        );
        Ok(Resource {
            key: ResourceKey {
                host: self.host.clone(),
                id: resource,
            },
            marker: std::marker::PhantomData,
        })
    }
}
