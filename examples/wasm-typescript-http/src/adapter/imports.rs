//! Consumer-independent invocation leases and a bounded import transport.
//!
//! Only tickets and typed messages enter the Send store. Local Context values stay
//! with the existing Tembra host. Consumers supply the operation and dispatch.

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    future::LocalBoxFuture,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};
use tembra::{Code, Context, Error, Result, core::ContextKey};
use wasmtime::{
    StoreLimits, StoreLimitsBuilder,
    component::{Resource, ResourceTable},
};

const ACTIVE: ContextKey<String> = ContextKey::new("probe/component-stack", 1);
static NEXT_REGISTRY: AtomicU64 = AtomicU64::new(1);

pub struct InvocationToken(u64);

pub struct Registry {
    id: u64,
    contexts: RefCell<BTreeMap<u64, Context>>,
}

impl Registry {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            id: NEXT_REGISTRY.fetch_add(1, Ordering::Relaxed),
            contexts: RefCell::default(),
        })
    }

    pub fn check_entry(&self, context: &Context) -> Result<()> {
        context.check()?;
        if context
            .value(&ACTIVE)?
            .unwrap_or_default()
            .contains(&format!("/{}/", self.id))
        {
            return Err(Error::new(
                Code::Unsupported,
                "component.reentry",
                "recursive entry into the active component is unsupported",
            ));
        }
        Ok(())
    }

    pub fn enter(self: &Rc<Self>, context: Context) -> Result<Lease> {
        self.check_entry(&context)?;
        let ticket = context.invocation()?.id;
        let mut active = context.value(&ACTIVE)?.unwrap_or_default();
        active.push_str(&format!("/{}/", self.id));
        let entered = context.with(&ACTIVE, active)?;
        let mut contexts = self.contexts.borrow_mut();
        if contexts.contains_key(&ticket) {
            return Err(Error::invalid(
                "component.lease",
                "invocation is already leased",
            ));
        }
        contexts.insert(ticket, entered);
        Ok(Lease {
            registry: self.clone(),
            ticket,
        })
    }

    fn resolve(&self, ticket: u64) -> Result<Context> {
        let context = self
            .contexts
            .borrow()
            .get(&ticket)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    Code::Expired,
                    "component.invocation",
                    "invocation lease has expired",
                )
            })?;
        context.check()?;
        Ok(context)
    }

    pub fn is_empty(&self) -> bool {
        self.contexts.borrow().is_empty()
    }
}

pub struct Lease {
    registry: Rc<Registry>,
    ticket: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.registry.contexts.borrow_mut().remove(&self.ticket);
    }
}

pub struct Request<O, R> {
    ticket: u64,
    operation: O,
    reply: oneshot::Sender<Result<R>>,
}

pub struct Imports<O, R> {
    table: ResourceTable,
    requests: mpsc::Sender<Request<O, R>>,
    pub limits: StoreLimits,
}

impl<O, R> Imports<O, R> {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Request<O, R>>) {
        let (requests, receive) = mpsc::channel(capacity);
        (
            Self {
                table: ResourceTable::new(),
                requests,
                limits: StoreLimitsBuilder::new()
                    .memory_size(64 * 1024 * 1024)
                    .table_elements(1024)
                    .build(),
            },
            receive,
        )
    }

    pub fn issue(&mut self, lease: &Lease) -> wasmtime::Result<Resource<InvocationToken>> {
        Ok(self.table.push(InvocationToken(lease.ticket))?)
    }

    pub fn revoke(&mut self, handle: Resource<InvocationToken>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    pub async fn request(&mut self, handle: Resource<InvocationToken>, operation: O) -> Result<R> {
        let ticket = self
            .table
            .get(&handle)
            .map_err(|error| Error::invalid("component.resource", error.to_string()))?
            .0;
        let (reply, receive) = oneshot::channel();
        self.requests
            .try_send(Request {
                ticket,
                operation,
                reply,
            })
            .map_err(|error| {
                Error::new(Code::Unavailable, "component.transport", error.to_string())
            })?;
        receive.await.map_err(|_| {
            Error::new(
                Code::Unavailable,
                "component.transport",
                "import receiver stopped",
            )
        })?
    }
}

/// Run beside the native host, without moving it into the component Store.
pub async fn serve<O, R>(
    mut receive: mpsc::Receiver<Request<O, R>>,
    registry: Rc<Registry>,
    dispatch: impl Fn(Context, O) -> LocalBoxFuture<'static, Result<R>>,
) {
    while let Some(request) = receive.next().await {
        let result = match registry.resolve(request.ticket) {
            Ok(context) => dispatch(context, request.operation).await,
            Err(error) => Err(error),
        };
        let _ = request.reply.send(result);
    }
}
