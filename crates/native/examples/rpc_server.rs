//! A configurable WebSocket server for the actual browser/Rust acceptance fixture.

use std::{cell::Cell, io::Write, rc::Rc, time::Duration};

use futures::FutureExt;
use serde_json::json;
use tembra_core::{
    Driver, Host, HostOptions,
    rpc::{ConnectionOptions, RootOrigin},
};
use tembra_native::{
    TokioDriver, WebSocketLink, WebSocketOptions,
    counter::{counter_contract, serve_counter},
    random_incarnation,
};
use tokio::{net::TcpListener, runtime::Builder};
use tokio_tungstenite::accept_async_with_config;

type AnyError = Box<dyn std::error::Error>;

struct Session(Rc<Cell<usize>>);
impl Drop for Session {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

fn main() -> std::result::Result<(), AnyError> {
    let mut arguments = std::env::args().skip(1);
    let address = arguments.next().unwrap_or_else(|| "127.0.0.1:0".into());
    if address == "--descriptor" {
        println!(
            "{}",
            serde_json::to_string(counter_contract()?.descriptor())?
        );
        return Ok(());
    }
    if arguments.next().is_some() {
        return Err("expected one listen address or --descriptor".into());
    }
    let driver = TokioDriver::new();
    Builder::new_current_thread().enable_all().build()?.block_on(driver.run_until(async {
        let host = Host::new(HostOptions::new("server", json!("server-runtime")), driver.clone());
        host.set_rpc_identity(RootOrigin::new("server", random_incarnation()?)?)?;
        let descriptor = serve_counter(&host)?;
        let listener = TcpListener::bind(address).await?;
        println!("{}", json!({"ready":true,"address":listener.local_addr()?.to_string(),"descriptor":descriptor.descriptor()}));
        std::io::stdout().flush()?;
        let sessions = Rc::new(Cell::new(0_usize));
        let stopped = tokio::signal::ctrl_c();
        futures::pin_mut!(stopped);
        loop {
            match futures::future::select(listener.accept().boxed_local(), stopped.as_mut()).await {
                futures::future::Either::Right((result, _)) => { result?; break; }
                futures::future::Either::Left((accepted, _)) => {
                    let (socket, _) = accepted?;
                    if sessions.get() >= 64 { drop(socket); continue; }
                    sessions.set(sessions.get() + 1);
                    let session = Session(sessions.clone());
                    let (host, driver) = (host.clone(), driver.clone());
                    let scheduler = driver.clone();
                    scheduler.spawn(async move {
                        let _session = session;
                        let result: std::result::Result<(), AnyError> = async {
                            let options = WebSocketOptions::default();
                            let socket = tokio::time::timeout(Duration::from_secs(5), accept_async_with_config(socket, Some(options.protocol_config()?))).await??;
                            // This fixture's embedding authenticates one fixed user. Production
                            // embeddings perform their own authentication before accepting RPC.
                            let (link, principal) = WebSocketLink::authenticate(socket, driver, options, async { Ok(json!("fixture-user")) }).await?;
                            let connection = host.accept_client(link, principal, ConnectionOptions::default())?;
                            connection.closed().await;
                            Ok(())
                        }.await;
                        if let Err(error) = result { eprintln!("connection refused: {error}"); }
                    }.boxed_local());
                }
            }
        }
        host.close().await?;
        Ok(())
    }))
}
