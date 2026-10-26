//! A logical root stays pinned until physical work settles.

use super::*;

impl Host {
    /// Issue a context for an active root and matching invocation.
    pub fn context(&self, root: core::Id, invocation: Option<core::Invocation>) -> Result<Context> {
        self.kernel().check_root(root)?;
        if invocation.as_ref().is_some_and(|call| call.root != root) {
            return Err(Error::invalid(
                Reason::Context,
                "invocation belongs to another root",
            ));
        }
        Ok(Context {
            host: self.clone(),
            root,
            frame: invocation.as_ref().map_or(0, |call| call.context),
            call: invocation,
        })
    }

    /// Run an operation while a guard retains its root lifetime.
    pub async fn root<T>(
        &self,
        principal: &str,
        operation: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>>,
    ) -> Result<T> {
        let root = self.kernel().begin_root(principal)?;
        let context = self.context(root, None)?;
        let _guard = RootGuard {
            host: self.clone(),
            root,
        };
        operation(context).await
    }
}

pub(super) struct RootGuard {
    pub(super) host: Host,
    pub(super) root: core::Id,
}

impl Drop for RootGuard {
    /// End the root's logical lifetime and dispatch pending cleanup.
    fn drop(&mut self) {
        if let Err(error) = self.host.kernel().end_root(self.root) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.flush();
    }
}

/// Contexts are issued by a host, and become unusable when their root or invocation ends.
#[derive(Clone)]
pub struct Context {
    frame: core::Id,
    pub(crate) host: Host,
    pub root: core::Id,
    call: Option<core::Invocation>,
}

impl Context {
    /// Read the frame identity for local adapter calls.
    pub fn frame(&self) -> core::Id {
        self.frame
    }

    /// Derive values without extending identity or invocation lifetime.
    pub fn with<T: core::ContextDatum>(&self, key: &core::ContextKey<T>, value: T) -> Result<Self> {
        self.check()?;
        let frame = self.host.kernel().derive_context(
            self.root,
            self.frame,
            self.call.as_ref().map(|call| call.id),
            key.contract(),
            value.into_value(),
        )?;
        Ok(Self {
            host: self.host.clone(),
            root: self.root,
            call: self.call.clone(),
            frame,
        })
    }

    /// Read typed metadata while this invocation remains valid.
    pub fn value<T: core::ContextDatum>(&self, key: &core::ContextKey<T>) -> Result<Option<T>> {
        self.check()?;
        if self.frame == 0 {
            return Ok(None);
        }
        self.host
            .kernel()
            .context_value(self.root, self.frame, &key.contract())?
            .map(T::from_value)
            .transpose()
    }

    /// Reject an expired root or completed invocation.
    pub fn check(&self) -> Result<()> {
        self.host.kernel().check_root(self.root)?;
        if let Some(call) = &self.call {
            self.host.kernel().invocation(call.id)?;
        }
        Ok(())
    }

    /// Signal root cancellation without releasing physical ownership.
    pub fn cancel(&self) -> Result<()> {
        self.host.kernel().cancel_root(self.root)
    }

    /// Read the identity retained by this root.
    pub fn principal(&self) -> Result<String> {
        self.host.kernel().root_principal(self.root)
    }

    /// Require an admitted call context for owner operations.
    pub fn invocation(&self) -> Result<core::Invocation> {
        self.call
            .clone()
            .ok_or_else(|| Error::invalid(Reason::Context, "an entered invocation is required"))
    }
}
