use super::*;

impl ActivityDeclarations {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            listeners: RefCell::default(),
            tasks: RefCell::default(),
        })
    }
    fn add(&self, name: String, declaration: ListenerDeclaration) -> Result<()> {
        if name.is_empty() || self.listeners.borrow().contains_key(&name) {
            return Err(RuntimeError::invalid(
                "duplicate",
                "listener names must be nonempty and unique",
            ));
        }
        if self.listeners.borrow().len() >= 128 {
            return Err(RuntimeError::invalid(
                "listener_limit",
                "object listener declaration limit reached",
            ));
        }
        self.listeners
            .borrow_mut()
            .insert(name, Rc::new(declaration));
        Ok(())
    }
    fn event<E: 'static>(
        &self,
        name: String,
        source: EventSource<E>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        let identity = Rc::new(source.clone());
        let compare = source.clone();
        let same = Rc::new(move |other: &dyn Any| {
            other
                .downcast_ref::<EventSource<E>>()
                .is_some_and(|other| compare.same_source(other))
        });
        let subscribe = Rc::new(
            move |sink: Rc<dyn Fn(Box<dyn Any>)>,
                  _authority: Option<&crate::state::GraphDeclarationSet>| {
                (source.subscribe)(Rc::new(move |event| sink(Box::new(event))))
            },
        );
        self.add(
            name,
            ListenerDeclaration {
                identity,
                same,
                subscribe,
                handler: Rc::new(move |event, context| match event.downcast::<E>() {
                    Ok(event) => handler(*event, context),
                    Err(_) => async {
                        Err(RuntimeError::invalid(
                            "event_type",
                            "event payload has wrong type",
                        ))
                    }
                    .boxed_local(),
                }),
                options,
                state: false,
            },
        )
    }
}
impl Declaration {
    /// Declares a named state listener. Initial and committed snapshots enter the managed queue.
    /// # Errors
    /// Refuses the exact declaration scope, duplicate names and listener limits before subscribe.
    pub fn on(
        &mut self,
        name: impl Into<String>,
        source: Readable,
        handler: impl Fn(Snapshot, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.on_with_options(name, source, handler, ListenerOptions::default())
    }
    /// Adds a state listener with a checked pending queue bound.
    /// # Errors
    /// Has the scope/name/limit failures of [`Self::on`].
    pub fn on_with_options(
        &mut self,
        name: impl Into<String>,
        source: Readable,
        handler: impl Fn(Snapshot, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        let identity = Rc::new(source.clone());
        let compare = source.clone();
        let same = Rc::new(move |other: &dyn Any| {
            other
                .downcast_ref::<Readable>()
                .is_some_and(|other| compare.same_source(other))
        });
        let subscribe = Rc::new(
            move |sink: Rc<dyn Fn(Box<dyn Any>)>,
                  authority: Option<&crate::state::GraphDeclarationSet>| {
                let handler = Rc::new(move |snapshot: Snapshot| {
                    sink(Box::new(snapshot));
                    Ok(())
                });
                let subscription =
                    if let Some(authority) = authority.filter(|set| set.owns(&source)) {
                        authority.subscribe(&source, handler)?
                    } else {
                        source.subscribe(move |snapshot| handler(snapshot))?
                    };
                Ok(Box::new(move || drop(subscription)) as Detach)
            },
        );
        self.activity_declarations()?.add(
            name.into(),
            ListenerDeclaration {
                identity,
                same,
                subscribe,
                handler: Rc::new(move |event, context| match event.downcast::<Snapshot>() {
                    Ok(event) => handler(*event, context),
                    Err(_) => async {
                        Err(RuntimeError::invalid(
                            "event_type",
                            "expected state snapshot",
                        ))
                    }
                    .boxed_local(),
                }),
                options,
                state: true,
            },
        )
    }
    /// Records a typed FIFO source without subscribing during factory execution.
    /// # Errors
    /// Refuses scope, duplicate name and checked declaration limits without source effects.
    pub fn on_event<E: 'static>(
        &mut self,
        name: impl Into<String>,
        source: EventSource<E>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.on_event_with_options(name, source, handler, ListenerOptions::default())
    }
    /// Records a typed FIFO source with an explicit checked queue bound.
    /// # Errors
    /// Has the declaration failures of [`Self::on_event`].
    pub fn on_event_with_options<E: 'static>(
        &mut self,
        name: impl Into<String>,
        source: EventSource<E>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        self.activity_declarations()?
            .event(name.into(), source, handler, options)
    }
    /// Selects a typed source only after the actual resource has opened.
    /// # Errors
    /// Refuses invalid declaration scope/name/limits before source selection.
    pub fn on_resource<T: 'static, E: 'static>(
        &mut self,
        name: impl Into<String>,
        source: Resource<T>,
        select: Rc<dyn Fn(Rc<T>) -> Result<EventSource<E>>>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.on_resource_with_options(name, source, select, handler, ListenerOptions::default())
    }
    /// Adds checked queue configuration to [`Self::on_resource`].
    /// # Errors
    /// Has the declaration failures of [`Self::on_resource`].
    pub fn on_resource_with_options<T: 'static, E: 'static>(
        &mut self,
        name: impl Into<String>,
        source: Resource<T>,
        select: Rc<dyn Fn(Rc<T>) -> Result<EventSource<E>>>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        let identity = Rc::new((source.clone(), select.clone()));
        let record = source.record.clone();
        let selector = select.clone();
        let same = Rc::new(move |other: &dyn Any| {
            other
                .downcast_ref::<(Resource<T>, Rc<dyn Fn(Rc<T>) -> Result<EventSource<E>>>)>()
                .is_some_and(|(other, select)| {
                    Rc::ptr_eq(&record, &other.record) && Rc::ptr_eq(&selector, select)
                })
        });
        let subscribe = Rc::new(
            move |sink: Rc<dyn Fn(Box<dyn Any>)>,
                  _authority: Option<&crate::state::GraphDeclarationSet>| {
                let selected = select(source.value()?)?;
                let detach = (selected.subscribe)(Rc::new(move |event| sink(Box::new(event))))?;
                Ok(Box::new(move || {
                    detach();
                    drop(selected);
                }) as Detach)
            },
        );
        self.activity_declarations()?.add(
            name.into(),
            ListenerDeclaration {
                identity,
                same,
                subscribe,
                handler: Rc::new(move |event, context| match event.downcast::<E>() {
                    Ok(event) => handler(*event, context),
                    Err(_) => async {
                        Err(RuntimeError::invalid(
                            "event_type",
                            "wrong resource event type",
                        ))
                    }
                    .boxed_local(),
                }),
                options,
                state: false,
            },
        )
    }
    /// Declares an ordinal task. Cancellation is cooperative; the Host owns physical completion.
    /// # Errors
    /// Refuses an expired declaration scope or more than 128 task declarations.
    pub fn task(
        &mut self,
        start: impl Fn(TaskScope) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        let declarations = self.activity_declarations()?;
        if declarations.tasks.borrow().len() >= 128 {
            return Err(RuntimeError::invalid(
                "task_limit",
                "object task declaration limit reached",
            ));
        }
        declarations.tasks.borrow_mut().push(Rc::new(start));
        Ok(())
    }
}
impl DeclarationRecorder {
    /// Records a state listener through this exact synchronous pass.
    /// # Errors
    /// Returns recorder scope and declaration errors without subscribing.
    pub fn on(
        &self,
        name: impl Into<String>,
        source: Readable,
        handler: impl Fn(Snapshot, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.declaration()?.on(name, source, handler)
    }
    /// # Errors
    /// Returns recorder scope and declaration errors without subscribing.
    pub fn on_with_options(
        &self,
        name: impl Into<String>,
        source: Readable,
        handler: impl Fn(Snapshot, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        self.declaration()?
            .on_with_options(name, source, handler, options)
    }
    /// Records a typed listener through this exact synchronous pass.
    /// # Errors
    /// Returns recorder scope and declaration errors without subscribing.
    pub fn on_event<E: 'static>(
        &self,
        name: impl Into<String>,
        source: EventSource<E>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.declaration()?.on_event(name, source, handler)
    }
    /// # Errors
    /// Returns recorder scope and declaration errors without subscribing.
    pub fn on_event_with_options<E: 'static>(
        &self,
        name: impl Into<String>,
        source: EventSource<E>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        self.declaration()?
            .on_event_with_options(name, source, handler, options)
    }
    /// Records a resource-selected listener through this exact synchronous pass.
    /// # Errors
    /// Returns recorder scope and declaration errors without selecting a resource value.
    pub fn on_resource<T: 'static, E: 'static>(
        &self,
        name: impl Into<String>,
        source: Resource<T>,
        select: Rc<dyn Fn(Rc<T>) -> Result<EventSource<E>>>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.declaration()?
            .on_resource(name, source, select, handler)
    }
    /// # Errors
    /// Returns recorder scope and declaration errors without selecting a resource value.
    pub fn on_resource_with_options<T: 'static, E: 'static>(
        &self,
        name: impl Into<String>,
        source: Resource<T>,
        select: Rc<dyn Fn(Rc<T>) -> Result<EventSource<E>>>,
        handler: impl Fn(E, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
        options: ListenerOptions,
    ) -> Result<()> {
        self.declaration()?
            .on_resource_with_options(name, source, select, handler, options)
    }
    /// Records an ordinal managed task through the actual declaration path.
    /// # Errors
    /// Returns recorder scope and task-count errors without starting work.
    pub fn task(
        &self,
        start: impl Fn(TaskScope) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<()> {
        self.declaration()?.task(start)
    }
}
