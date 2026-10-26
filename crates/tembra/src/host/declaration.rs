//! Build native state and resources during synchronous declaration.

use super::*;

#[derive(Clone)]
pub struct Declaration {
    pub(super) host: Host,
    pub(super) object: core::Id,
    pub(super) owner: core::Owner,
    pub(super) orb: core::Id,
    pub(super) draft: Rc<super::scopes::Draft>,
}

impl Declaration {
    pub(super) fn check_declaration(&self) -> Result<()> {
        if self.draft.sealed.get() {
            Err(Error::invalid(Reason::ScopeClosed, "declaration has ended"))
        } else {
            Ok(())
        }
    }

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
        self.check_declaration()?;
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
        self.check_declaration()?;
        if self.draft.resource_names.borrow().contains(name) {
            return Err(Error::invalid(
                Reason::Resource,
                "resource names must be unique in a declaration",
            ));
        }
        if self.owner == core::Owner::Host && self.host.0.host_resources.borrow().contains_key(name)
        {
            return Err(Error::invalid(
                Reason::Resource,
                "host resource names must be unique",
            ));
        }
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
            self.owner,
            name.into(),
            version,
            false,
            dependencies
                .iter()
                .map(|dependency| dependency.id)
                .collect(),
        )?;
        self.draft.resource_names.borrow_mut().insert(name.into());
        self.draft.resources.borrow_mut().push(record.id);
        if self.owner == core::Owner::Host {
            self.host.0.host_resources.borrow_mut().insert(
                name.into(),
                ResourceKey {
                    host: self.host.clone(),
                    id: record.id,
                    version,
                },
            );
        }
        let host = self.host.clone();
        let resource = record.id;
        let signal = ResourceSignal::new("resource");
        self.host
            .0
            .cancellations
            .borrow_mut()
            .insert(resource, signal.clone());
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
                version,
            },
            marker: std::marker::PhantomData,
        })
    }
}
