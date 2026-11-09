//! Registered diagnostics and synchronous callbacks carry no ambient filesystem or network.
use crate::protocol::{LogLevel, Request};
use rquickjs::{Ctx, Function, snapshot::SnapshotHost};

fn active() -> bool {
    super::instance_vm().is_some_and(|vm| vm.accepting.get())
}
fn sync(packet: String) -> rquickjs::Result<String> {
    if !active() {
        return Err(super::error("callback scope is closed".into()));
    }
    super::host_sync(&packet).map_err(super::error)
}
/// Emit one attributed diagnostic without issuing execution authority.
pub(super) fn log(level: LogLevel, message: String) -> Result<String, String> {
    super::host_sync(&Request::Console { level, message }.encode())
}
fn write_log(level: String, message: String) -> rquickjs::Result<()> {
    let level = serde_json::from_value(serde_json::Value::String(level))
        .map_err(|_| super::error("invalid console level".into()))?;
    log(level, message).map(|_| ()).map_err(super::error)
}
/// Stable registry keys must match when restoring a heap from this engine build.
pub(super) fn host() -> SnapshotHost {
    SnapshotHost::new()
        .function("tembra.hostSync", |ctx| Function::new(ctx.clone(), sync))
        .function("tembra.scopeActive", |ctx| {
            Function::new(ctx.clone(), active)
        })
        .function("tembra.writeLog", |ctx| {
            Function::new(ctx.clone(), write_log)
        })
}
pub(super) fn install(ctx: &Ctx<'_>, host: &SnapshotHost) -> rquickjs::Result<()> {
    let natives = host.natives(ctx)?;
    for (global, key) in [
        ("__hostSync", "tembra.hostSync"),
        ("__scopeActive", "tembra.scopeActive"),
        ("__writeLog", "tembra.writeLog"),
    ] {
        ctx.globals().set(global, natives.function(key)?)?;
    }
    Ok(())
}
