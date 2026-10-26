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
    pub(crate) host: Host,
    pub root: core::Id,
    call: Option<core::Invocation>,
}

impl Context {
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
