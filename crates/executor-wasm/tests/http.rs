//! The fixture server runs natively; the TypeScript client frames HTTP inside Wasm.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tembra::{Host, Orb, Service};
use tembra_executor_wasm::{
    Binding, Contract, Engine, Grants, Limits, NetworkGrant, PreparedSource,
};

struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
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
                received
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                let body = r#"{"values":[3,4,5]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
            address,
            requests,
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

/// Combine a real HTTP request, npm calculation, native interop and managed state.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_ORB"]
fn typescript_owns_http_and_the_host_only_grants_transport() {
    let server = Server::start();
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let prepared: PreparedSource = serde_json::from_slice(
        &std::fs::read(std::env::var_os("TEMBRA_WASM_ORB").expect("TEMBRA_WASM_ORB")).unwrap(),
    )
    .unwrap();
    let source = prepared.source;
    let engine = Engine::new(&component, Limits::default())
        .unwrap()
        .network("tcp")
        .unwrap();
    let contract = Contract::from_bytes(&prepared.contract, Some("online-calculator")).unwrap();
    let bindings = BTreeMap::from([
        (
            "example:integrated/arithmetic@1.0.0".into(),
            Binding::Orb {
                name: "native.math".into(),
                api_version: 1,
                world: "arithmetic-provider".into(),
            },
        ),
        (
            "wasi:sockets/types@0.3.0".into(),
            Binding::Host {
                provider: "tcp".into(),
            },
        ),
    ]);
    assert!(
        engine
            .prepare(&contract, "online", 1, &source, &bindings)
            .is_err()
    );
    let grants = Grants {
        network: vec![NetworkGrant {
            provider: "tcp".into(),
            connect: vec![server.address],
        }],
        ..Grants::default()
    };
    let orb = engine
        .prepare_with_grants(&contract, "online", 1, &source, &bindings, &grants)
        .unwrap();
    let native = Orb::new("native.math", 1).service(
        Service::new("native.math::example:integrated/arithmetic@1.0.0", |_| {
            Ok(())
        })
        .method(
            "double",
            |_, _, (value,): (i32,)| async move { Ok(value * 2) },
        ),
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![native, orb]).unwrap();
    let reference = Rc::new(
        host.get(
            "online::example:integrated/calculation@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    let url = format!("http://{}/numbers", server.address);
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "fetch-and-sum", json!([url]))
                    .await?,
                24
            );
            assert_eq!(
                called.call::<_, Value>(&context, "read", json!([])).await?,
                24
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
    assert!(server.requests.lock().unwrap()[0].starts_with("GET /numbers HTTP/1.1\r\n"));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
