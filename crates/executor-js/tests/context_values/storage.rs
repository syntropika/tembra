//! Test fixture: storage owns pending writes, revocation and cleanup.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};
use tembra::{
    Context, Error, Facet, Host, Service,
    core::{ContextKey, OpaqueContextValue},
};
const SCOPE: ContextKey<OpaqueContextValue> = ContextKey::new("storage/pending", 1);

struct Pending {
    root: u64,
    principal: String,
    open: bool,
    writes: Vec<i64>,
}

#[derive(Default)]
struct Store {
    scopes: BTreeMap<u64, Pending>,
    rows: Vec<i64>,
    commits: usize,
    delayed: usize,
}

struct ScopeLease {
    store: Weak<RefCell<Store>>,
    id: u64,
}

impl Drop for ScopeLease {
    /// Release abandoned pending writes when the last lease ends.
    fn drop(&mut self) {
        if let Some(store) = self.store.upgrade() {
            store.borrow_mut().scopes.remove(&self.id);
        }
    }
}

/// Require an open scope owned by this operation.
fn check<'a>(store: &'a mut Store, ctx: &Context) -> tembra::Result<&'a mut Pending> {
    ctx.check()?;
    let id = ctx
        .value(&SCOPE)?
        .ok_or_else(|| Error::invalid("storage_scope", "scope is required"))?
        .id;
    let pending = store.scopes.get_mut(&id).ok_or_else(|| {
        Error::invalid(
            "storage_scope_closed",
            "not issued by storage or already closed",
        )
    })?;
    if !pending.open || pending.root != ctx.root || pending.principal != ctx.principal()? {
        return Err(Error::invalid(
            "storage_scope_closed",
            "scope closed or belongs to another operation",
        ));
    }
    Ok(pending)
}

/// Install a storage fixture without adding storage policy to Tembra.
pub fn install(host: &Host) {
    let store = Rc::new(RefCell::new(Store::default()));
    let service = Service::new("storage", move |_| Ok(store.clone()))
        .method("open", |store, ctx, _args: Vec<serde_json::Value>| async move {
            let id = ctx.invocation()?.id;
            let pending = Pending {
                root: ctx.root,
                principal: ctx.principal()?,
                open: true,
                writes: vec![],
            };
            store.borrow_mut().scopes.insert(id, pending);
            let lease = ScopeLease { store: Rc::downgrade(&store), id };
            let scoped = ctx.with(&SCOPE, OpaqueContextValue::new(id, lease))?;
            Ok(scoped.frame())
        })
        .method("append", |store, ctx, (value,): (i64,)| async move {
            check(&mut store.borrow_mut(), &ctx)?.writes.push(value);
            Ok(value)
        })
        .method("appendDelayed", |store, ctx, (value,): (i64,)| async move {
            check(&mut store.borrow_mut(), &ctx)?;
            store.borrow_mut().delayed += 1;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let result = check(&mut store.borrow_mut(), &ctx).map(|scope| scope.writes.push(value));
            store.borrow_mut().delayed -= 1;
            result.map(|_| value)
        })
        .method("close", |store, ctx, (commit,): (bool,)| async move {
            let mut store = store.borrow_mut();
            let pending = check(&mut store, &ctx)?;
            pending.open = false;
            let writes = std::mem::take(&mut pending.writes);
            let id = ctx.value(&SCOPE)?.unwrap().id;
            store.scopes.remove(&id);
            // Commit only after asynchronous work has assembled pending writes.
            if commit {
                store.rows.extend(writes);
                store.commits += 1;
            }
            Ok(())
        })
        .method("stats", |store, _ctx, _args: Vec<serde_json::Value>| async move {
            let store = store.borrow();
            Ok(serde_json::json!({"rows":store.rows,"commits":store.commits,"backendTransactionsOpen":0,"delayed":store.delayed,"pendingScopes":store.scopes.len()}))
        });
    host.install(vec![Facet::new("storage", 1).service(service)])
        .unwrap();
}
