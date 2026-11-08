//! Stable registered natives can be reconstructed without persisting Rust closures or host authority.
use rquickjs::{Ctx, Function, snapshot::SnapshotHost};
use std::io::{Read, Write};

/// Resolve current admission from the new instance rather than capturing a previous runtime's cells.
fn active() -> bool {
    super::instance_vm().is_some_and(|vm| vm.accepting.get())
}
fn sync(packet: String) -> rquickjs::Result<String> {
    if !active() {
        return Err(super::error("callback scope is closed".into()));
    }
    super::host_sync(&packet).map_err(super::error)
}
fn capability(operation: &str) -> rquickjs::Result<()> {
    if !active() {
        return Err(super::error("callback scope is closed".into()));
    }
    super::host_sync(&serde_json::json!({"operation":operation}).to_string())
        .map(|_| ())
        .map_err(super::error)
}
fn read_text(path: String) -> rquickjs::Result<String> {
    capability("filesystem")?;
    let file = std::fs::File::open(path).map_err(|e| super::error(e.to_string()))?;
    let mut value = String::new();
    file.take((1 << 20) + 1)
        .read_to_string(&mut value)
        .map_err(|e| super::error(e.to_string()))?;
    if value.len() > 1 << 20 {
        return Err(super::error("file exceeds the text read limit".into()));
    }
    Ok(value)
}
fn write_text(path: String, value: String) -> rquickjs::Result<()> {
    capability("filesystem")?;
    if value.len() > 1 << 20 {
        return Err(super::error("file exceeds the text write limit".into()));
    }
    std::fs::write(path, value).map_err(|e| super::error(e.to_string()))
}
fn write_log(level: String, message: String) -> rquickjs::Result<()> {
    super::host_sync(
        &serde_json::json!({"operation":"console","level":level,"message":message}).to_string(),
    )
    .map(|_| ())
    .map_err(super::error)
}
fn exchange(ip: String, port: u16, request: String) -> rquickjs::Result<String> {
    capability("network")?;
    if request.len() > 1 << 20 {
        return Err(super::error("transport request exceeds its limit".into()));
    }
    let address = std::net::SocketAddr::new(
        ip.parse()
            .map_err(|_| super::error("transport requires a literal IP address".into()))?,
        port,
    );
    let mut socket =
        std::net::TcpStream::connect(address).map_err(|e| super::error(e.to_string()))?;
    socket
        .write_all(request.as_bytes())
        .map_err(|e| super::error(e.to_string()))?;
    socket
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| super::error(e.to_string()))?;
    let mut response = String::new();
    socket
        .take((1 << 20) + 1)
        .read_to_string(&mut response)
        .map_err(|e| super::error(e.to_string()))?;
    if response.len() > 1 << 20 {
        return Err(super::error("transport response exceeds its limit".into()));
    }
    capability("network")?;
    Ok(response)
}

/// Register the exact same native keys, in the same order, for load and restore.
pub(super) fn host() -> SnapshotHost {
    SnapshotHost::new()
        .function("tembra.hostSync", |ctx| Function::new(ctx.clone(), sync))
        .function("tembra.scopeActive", |ctx| {
            Function::new(ctx.clone(), active)
        })
        .function("tembra.readText", |ctx| {
            Function::new(ctx.clone(), read_text)
        })
        .function("tembra.writeText", |ctx| {
            Function::new(ctx.clone(), write_text)
        })
        .function("tembra.writeLog", |ctx| {
            Function::new(ctx.clone(), write_log)
        })
        .function("tembra.tcpExchange", |ctx| {
            Function::new(ctx.clone(), exchange)
        })
}
/// Publish only registered functions; no anonymous Rust closure may enter a durable heap.
pub(super) fn install(ctx: &Ctx<'_>, host: &SnapshotHost) -> rquickjs::Result<()> {
    let natives = host.natives(ctx)?;
    for (global, key) in [
        ("__hostSync", "tembra.hostSync"),
        ("__scopeActive", "tembra.scopeActive"),
        ("__readText", "tembra.readText"),
        ("__writeText", "tembra.writeText"),
        ("__writeLog", "tembra.writeLog"),
        ("__tcpExchange", "tembra.tcpExchange"),
    ] {
        ctx.globals().set(global, natives.function(key)?)?;
    }
    Ok(())
}
