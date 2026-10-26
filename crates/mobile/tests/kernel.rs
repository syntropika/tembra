use serde_json::{Value, json};
use tembra_mobile::{MobileError, NativeKernel};

#[test]
fn preserves_metadata_envelopes_and_isolates_hosts() {
    let first = NativeKernel::new().unwrap();
    let second = NativeKernel::new().unwrap();
    for kernel in [&first, &second] {
        let features: Value = serde_json::from_str(
            &kernel
                .request(json!({"op": "features"}).to_string())
                .unwrap(),
        )
        .unwrap();
        assert!(features["ok"].is_object());
        let allocated: Value = serde_json::from_str(
            &kernel
                .request(json!({"op": "allocate"}).to_string())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(allocated["ok"], 1);
        let invalid: Value =
            serde_json::from_str(&kernel.request("invalid".into()).unwrap()).unwrap();
        assert!(invalid["error"].is_object());
    }
}

#[test]
fn concurrent_requests_and_close_are_safe() {
    let kernel = NativeKernel::new().unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let kernel = kernel.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    match kernel.request(json!({"op": "allocate"}).to_string()) {
                        Ok(response) => assert!(
                            serde_json::from_str::<Value>(&response).unwrap()["ok"].is_number()
                        ),
                        Err(MobileError::Closed) => break,
                        Err(error) => panic!("unexpected failure: {error}"),
                    }
                }
            })
        })
        .collect();
    kernel.shutdown();
    kernel.shutdown();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(matches!(
        kernel.request("{}".into()),
        Err(MobileError::Closed)
    ));
}
