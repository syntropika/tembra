//! The TypeScript guest fetches through host-owned WASI HTTP and streams SSE before EOF.
mod support;
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tembra::Host;
use tembra_executor_wasm::{Engine, Grants, HttpGrant, Install, Limits};

struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    event_closed: std::sync::mpsc::Receiver<()>,
}

impl Server {
    /// Serve a local fixture and retain the exact request received from the guest.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let ended = stop.clone();
        let received = requests.clone();
        let (event_closed, observed_close) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            while !ended.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture listener failed: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                let mut request = Vec::new();
                let mut bytes = [0; 2048];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut bytes).unwrap();
                    assert!(
                        count > 0 && request.len() + count <= 8192,
                        "invalid fixture request"
                    );
                    request.extend_from_slice(&bytes[..count]);
                }
                let event = request.starts_with(b"GET /events ");
                received
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                if event {
                    socket.write_all("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: hé🌍\n\n".as_bytes()).unwrap();
                    // No EOF is sent: the guest must consume one event and close its reader.
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    assert_eq!(socket.read(&mut bytes).unwrap(), 0);
                    event_closed.send(()).unwrap();
                    continue;
                }
                let body = r#"{"values":[3,4,5]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x};part=1\r\n{body}\r\n0\r\nX-Finished: yes\r\n\r\n",
                    body.len(),
                );
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
            address,
            requests,
            event_closed: observed_close,
        }
    }
}

impl Drop for Server {
    /// Stop and join only this test's fixture server, including assertion failures.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Combine a real HTTP request, native interop and an SSE stream that closes before EOF.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_FIXTURES"]
fn typescript_owns_http_and_the_host_only_grants_transport() {
    for interpreted in [false, true] {
        run(interpreted);
    }
}

fn run(interpreted: bool) {
    let server = Server::start();
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let bytes = std::fs::read(
        std::path::PathBuf::from(
            std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES"),
        )
        .join("online.wasm"),
    )
    .unwrap();
    let mode = if interpreted { "pulley" } else { "cranelift" };
    let preparing = Instant::now();
    let engine = if interpreted {
        Engine::interpreted(Limits::default())
    } else {
        Engine::new(Limits::default())
    }
    .unwrap()
    .interpreter(&component)
    .unwrap_or_else(|error| {
        panic!(
            "HTTP fixture mode={mode} phase=prepare elapsed={:?}: {error:?}",
            preparing.elapsed()
        )
    })
    .network("tcp")
    .unwrap();
    let prepared_in = preparing.elapsed();
    let install =
        || Install::default().requires("example:integrated/arithmetic@1.0.0", "native.math", 1);
    assert!(engine.load(&bytes, install()).is_err());
    let grants = Grants {
        http: vec![HttpGrant {
            provider: "tcp".into(),
            origins: vec![format!("http://{}", server.address)],
        }],
        ..Grants::default()
    };
    let mut allowed = install();
    allowed.grants = grants;
    let loading = Instant::now();
    let orb = engine.load(&bytes, allowed).unwrap_or_else(|error| {
        panic!(
            "HTTP fixture mode={mode} phase=load elapsed={:?} prepare={prepared_in:?}: {error:?}",
            loading.elapsed()
        )
    });
    let native = support::native_math("native.math");
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![native, orb]).unwrap();
    let reference = Rc::new(
        host.get(
            "online::example:integrated/network@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    let url = format!("http://{}/numbers", server.address);
    let events_url = format!("http://{}/events", server.address);
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "fetch-and-sum", json!([url]))
                    .await?,
                24
            );
            assert_eq!(
                called
                    .call::<_, Value>(&context, "first-event", json!([events_url]))
                    .await?,
                "hé🌍"
            );
            assert!(
                called
                    .call::<_, Value>(
                        &context,
                        "fetch-and-sum",
                        json!(["http://127.0.0.1:1/forbidden"])
                    )
                    .await
                    .is_err()
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    server
        .event_closed
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(server.requests.lock().unwrap()[0].starts_with("GET /numbers HTTP/1.1\r\n"));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
