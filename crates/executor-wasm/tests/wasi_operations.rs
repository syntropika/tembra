//! Operation-level standard WASI qualification through real compiled guest bindings on both
//! native execution strategies. Reports remain ordinary typed Rust values, including exact u64s.
use futures::{FutureExt, executor::LocalPool};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{rc::Rc, sync::Arc, time::Duration};
use tembra::Host;
use tembra_executor_wasm::{
    Cli, Contract, Deterministic, Engine, FilesystemGrant, Grants, HostMonotonicClock,
    HostWallClock, HttpGrant, Input, Install, Limits, Output, SystemCapability,
};

const EMPTY: [u8; 0] = [];

fn component(variable: &str) -> Vec<u8> {
    let path = std::env::var_os(variable)
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| panic!("{variable} must name a compiled qualification fixture"));
    std::fs::read(path).unwrap()
}

fn artifact(bytes: &[u8], directory: &str, world: &str, orb: &str) -> Vec<u8> {
    Contract::from_dir(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/wit")
            .join(directory),
        Some(world),
    )
    .unwrap()
    .component_artifact(bytes, orb, 1, &Default::default())
    .unwrap()
}

fn engines() -> [(&'static str, Engine); 2] {
    [
        ("cranelift", Engine::new(Limits::default()).unwrap()),
        ("pulley", Engine::interpreted(Limits::default()).unwrap()),
    ]
}

struct Orb {
    pool: LocalPool,
    host: Host,
    reference: Rc<tembra::Reference>,
}

impl Orb {
    fn install(engine: &Engine, artifact: &[u8], install: Install, service: &str) -> Self {
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![engine.load(artifact, install).unwrap()])
            .unwrap();
        let reference = Rc::new(host.get(service, vec!["main".into()], "test").unwrap());
        pool.run_until(reference.ready()).unwrap();
        Self {
            pool,
            host,
            reference,
        }
    }

    fn call<A: Serialize + 'static, R: DeserializeOwned + 'static>(
        &mut self,
        method: &'static str,
        arguments: A,
    ) -> tembra::Result<R> {
        let reference = self.reference.clone();
        self.pool.run_until(self.host.root("test", move |context| {
            async move { reference.call_portable(&context, method, arguments).await }.boxed_local()
        }))
    }

    fn close(self, engine: &Engine) {
        self.reference.release().unwrap();
        let mut pool = self.pool;
        pool.run_until(self.host.close()).unwrap();
        assert_eq!(engine.instances(), 0, "all compiled Stores retired");
        assert_eq!(engine.resources(), 0, "no issued resources remain");
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "tag", rename_all = "lowercase")]
enum Reply<T> {
    Ok { val: T },
    Err { val: String },
}

impl<T> Reply<T> {
    fn value(self) -> T {
        match self {
            Self::Ok { val } => val,
            Self::Err { val } => panic!("guest qualification failed: {val}"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "tag", rename_all = "lowercase")]
enum Done {
    Ok,
    Err { val: String },
}

impl Done {
    fn success(self) {
        if let Self::Err { val } = self {
            panic!("guest qualification failed: {val}");
        }
    }
}

fn system_install() -> Install {
    let mut install = Install::default();
    install.grants.system = [
        SystemCapability::Clocks,
        SystemCapability::Random,
        SystemCapability::Cli,
    ]
    .into();
    install
}

const SECONDS: u64 = (1 << 53) + 1;
const MONOTONIC: u64 = (1 << 53) + 3;
const SEED: u128 = ((u64::MAX as u128) << 64) | u64::MAX as u128;

struct Wall;
impl HostWallClock for Wall {
    fn resolution(&self) -> Duration {
        Duration::from_nanos(17)
    }
    fn now(&self) -> Duration {
        Duration::new(SECONDS, 999_999_999)
    }
}

struct Monotonic;
impl HostMonotonicClock for Monotonic {
    fn resolution(&self) -> u64 {
        19
    }
    fn now(&self) -> u64 {
        MONOTONIC
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ClockReport {
    seconds: i64,
    nanoseconds: u32,
    system_resolution: u64,
    monotonic: u64,
    monotonic_resolution: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RandomReport {
    secure: Vec<u8>,
    insecure: Vec<u8>,
    secure_word: u64,
    insecure_word: u64,
    seed: (u64, u64),
    next_seed: (u64, u64),
}

#[derive(Debug, Deserialize, PartialEq)]
struct EnvironmentReport {
    environment: Vec<(String, String)>,
    arguments: Vec<String>,
    cwd: Option<String>,
    terminals: (bool, bool, bool),
}

#[derive(Debug, Deserialize)]
struct TransferReport {
    bytes: u64,
    checksum: u64,
}

/// Actual generated imports observe configured sources, binary streams, exact integers and waits.
#[test]
#[ignore = "requires TEMBRA_WASM_SYSTEM"]
fn compiled_system_operations_preserve_values_and_binary_cli() {
    let bytes = component("TEMBRA_WASM_SYSTEM");
    let artifact = artifact(&bytes, "system", "system", "system");
    for (strategy, engine) in engines() {
        let directory = tempfile::tempdir().unwrap();
        let stdout = directory.path().join("stdout");
        let stderr = directory.path().join("stderr");
        let input: Vec<u8> = (0..150_007).map(|index| (index % 251) as u8).collect();
        let engine = engine
            .wall_clock(|| Wall)
            .monotonic_clock(|| Monotonic)
            // A deterministic source is injected only to qualify the binding's conversion.
            .secure_random(|| Deterministic::new(vec![255]))
            .insecure_random(|| Deterministic::new(vec![128]))
            .insecure_random_seed(|| SEED);
        let mut install = system_install();
        install.cli = Cli {
            arguments: vec!["orb".into(), "--flag=unicode".into()],
            environment: vec![("MODE".into(), "a=b".into())],
            initial_cwd: Some("/work".into()),
            stdin: Input::Bytes(input.clone()),
            stdout: Output::File(stdout.clone()),
            stderr: Output::File(stderr.clone()),
        };
        let mut orb = Orb::install(
            &engine,
            &artifact,
            install,
            "system::example:system/probe@1.0.0",
        );
        assert_eq!(
            orb.call::<_, ClockReport>("clocks", EMPTY).unwrap(),
            ClockReport {
                seconds: SECONDS as i64,
                nanoseconds: 999_999_999,
                system_resolution: 17,
                monotonic: MONOTONIC,
                monotonic_resolution: 19,
            },
            "{strategy} exact clock report"
        );
        for length in [0_u64, 17] {
            let report: RandomReport = orb.call("random", (length,)).unwrap();
            assert_eq!(report.secure, vec![255; length as usize]);
            assert_eq!(report.insecure, vec![128; length as usize]);
            assert_eq!(report.secure_word, u64::MAX);
            assert_eq!(report.insecure_word, 0x8080_8080_8080_8080);
            assert_eq!(report.seed, (u64::MAX, u64::MAX));
            assert_eq!(report.next_seed, report.seed);
        }
        assert_eq!(
            orb.call::<_, EnvironmentReport>("environment", EMPTY)
                .unwrap(),
            EnvironmentReport {
                environment: vec![("MODE".into(), "a=b".into())],
                arguments: vec!["orb".into(), "--flag=unicode".into()],
                cwd: Some("/work".into()),
                terminals: (true, true, true),
            }
        );
        orb.call::<_, ()>("wait-until", (0_u64,)).unwrap();
        orb.call::<_, ()>("wait-for", (1_000_000_u64,)).unwrap();
        let report = orb
            .call::<_, Reply<TransferReport>>("transfer", EMPTY)
            .unwrap()
            .value();
        assert_eq!(report.bytes, input.len() as u64);
        assert_eq!(
            report.checksum,
            input.iter().fold(0_u64, |sum, byte| sum
                .wrapping_mul(31)
                .wrapping_add(u64::from(*byte)))
        );
        assert_eq!(std::fs::read(&stdout).unwrap(), input);
        assert_eq!(std::fs::read(&stderr).unwrap(), [0, 255, 128, 10]);
        orb.call::<_, Done>("write", (false, vec![0_u8, 255]))
            .unwrap()
            .success();
        orb.call::<_, Done>("write", (true, vec![128_u8, 0]))
            .unwrap()
            .success();
        orb.call::<_, Done>("write-pair", (vec![3_u8, 2, 1],))
            .unwrap()
            .success();
        assert_eq!(
            std::fs::read(&stdout).unwrap(),
            [input.as_slice(), &[0, 255, 3, 2, 1]].concat()
        );
        assert_eq!(
            std::fs::read(&stderr).unwrap(),
            [0, 255, 128, 10, 128, 0, 3, 2, 1]
        );
        assert_eq!(orb.call::<_, String>("ping", EMPTY).unwrap(), "alive");
        assert!(orb.host.errors().is_empty());
        orb.close(&engine);

        // Dropping an unread input stream settles its companion future while the Store lives.
        let mut install = system_install();
        install.cli.stdin = Input::Bytes(vec![1; 70_000]);
        let mut orb = Orb::install(
            &engine,
            &artifact,
            install,
            "system::example:system/probe@1.0.0",
        );
        assert_eq!(
            orb.call::<_, EnvironmentReport>("environment", EMPTY)
                .unwrap(),
            EnvironmentReport {
                environment: vec![],
                arguments: vec![],
                cwd: None,
                terminals: (true, true, true),
            },
            "host environment and arguments are never inherited"
        );
        assert_eq!(
            orb.call::<_, Reply<u32>>("read-drop", EMPTY)
                .unwrap()
                .value(),
            1
        );
        orb.close(&engine);
    }
}

/// A CLI grant never implicitly grants clocks or random; configured providers remain untouched
/// on refused installations. All exit entrypoints fail only their own installed revision.
#[test]
#[ignore = "requires TEMBRA_WASM_SYSTEM"]
fn compiled_system_grants_and_exits_remain_contained() {
    let bytes = component("TEMBRA_WASM_SYSTEM");
    let artifact = artifact(&bytes, "system", "system", "system");
    for (strategy, engine) in engines() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("never-created");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let engine = engine.wall_clock(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Wall
        });
        for denied in [
            vec![],
            vec![SystemCapability::Cli],
            vec![SystemCapability::Clocks, SystemCapability::Random],
            vec![SystemCapability::Clocks, SystemCapability::Cli],
        ] {
            let mut install = Install::default();
            install.grants.system = denied.into_iter().collect();
            install.cli.stdout = Output::File(output.clone());
            assert!(engine.load(&artifact, install).is_err(), "{strategy}");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!output.exists(), "ungranted CLI files were never opened");
        for (success, status) in [(None, 7), (Some(true), 0), (Some(false), 1)] {
            let mut orb = Orb::install(
                &engine,
                &artifact,
                system_install(),
                "system::example:system/probe@1.0.0",
            );
            let error = match success {
                Some(success) => orb.call::<_, ()>("exit", (success,)),
                None => orb.call::<_, ()>("stop", (7_u8,)),
            }
            .unwrap_err();
            assert!(
                error.to_string().contains(&format!(
                    "orb exited with status {status}; component retired"
                )),
                "{strategy}: {error}"
            );
            assert!(orb.call::<_, String>("ping", EMPTY).is_err());
            orb.close(&engine);
        }
    }
}

/// Cancellation reaches a real upstream clock wait, physically retires that invocation, and
/// leaves its installed component available for another callback.
#[test]
#[ignore = "requires TEMBRA_WASM_SYSTEM"]
fn compiled_clock_wait_cancellation_releases_the_callback() {
    use futures::task::LocalSpawnExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Observed(Arc<AtomicUsize>);
    impl HostMonotonicClock for Observed {
        fn resolution(&self) -> u64 {
            1
        }
        fn now(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst);
            0
        }
    }
    let bytes = component("TEMBRA_WASM_SYSTEM");
    let artifact = artifact(&bytes, "system", "system", "system");
    for (strategy, engine) in engines() {
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = reads.clone();
        let engine = engine.monotonic_clock(move || Observed(clock_reads.clone()));
        for method in ["wait-until", "wait-for"] {
            let mut orb = Orb::install(
                &engine,
                &artifact,
                system_install(),
                "system::example:system/probe@1.0.0",
            );
            let baseline = reads.load(Ordering::SeqCst);
            let (issued, context) = futures::channel::oneshot::channel();
            let (finished, finish) = futures::channel::oneshot::channel();
            let host = orb.host.clone();
            let reference = orb.reference.clone();
            orb.pool
                .spawner()
                .spawn_local(async move {
                    let result = host
                        .root("test", move |context| {
                            async move {
                                assert!(issued.send(context.clone()).is_ok());
                                reference
                                    .call_portable::<_, ()>(&context, method, (60_000_000_000_u64,))
                                    .await
                            }
                            .boxed_local()
                        })
                        .await;
                    finished.send(result).unwrap();
                })
                .unwrap();
            let context = orb.pool.run_until(context).unwrap();
            orb.pool.run_until(async {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while reads.load(Ordering::SeqCst) == baseline {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "clock wait never entered"
                    );
                    futures_timer::Delay::new(Duration::from_millis(1)).await;
                }
            });
            let cancelled = std::time::Instant::now();
            context.cancel().unwrap();
            assert!(
                context.check().is_err(),
                "the cancelled context has no authority"
            );
            let result = orb.pool.run_until(finish).unwrap();
            let elapsed = cancelled.elapsed();
            let next = orb.call::<_, String>("ping", EMPTY);
            let errors = orb.host.errors();
            orb.close(&engine);
            // A standard clock wait has a unit result; cooperative cancellation ends the wait,
            // while authority revocation remains visible through the managed context above.
            assert!(result.is_ok(), "{strategy}/{method}: {result:?}");
            assert!(
                elapsed < Duration::from_secs(2),
                "{strategy}/{method}: cancellation physically settled after {elapsed:?}; next callback: {next:?}"
            );
            assert_eq!(next.unwrap(), "alive");
            assert!(errors.is_empty());
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesystemReport {
    length: u64,
    large_offset: String,
    listing: Vec<String>,
    directory_rename: String,
    hash_lower: u64,
    hash_upper: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesystemErrors {
    checked: Vec<String>,
    hard_link: String,
    symlink: String,
    timestamp: String,
    timestamp_at: String,
    timestamp_missing: String,
}

#[cfg(target_os = "linux")]
fn descriptors_beneath(directory: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| target.starts_with(directory))
        .count()
}

/// Descriptor/path operations execute against real directories, and a guest-held read stream
/// stays valid after its file descriptor is dropped and across two separate managed roots.
#[test]
#[ignore = "requires TEMBRA_WASM_FILESYSTEM"]
fn compiled_filesystem_operations_and_retained_streams() {
    let bytes = component("TEMBRA_WASM_FILESYSTEM");
    let artifact = artifact(&bytes, "filesystem", "filesystem", "files");
    for (strategy, engine) in engines() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("input.txt"), b"seeded").unwrap();
        let engine = engine.filesystem("data", directory.path()).unwrap();
        let mut install = Install::default();
        install.grants.filesystem = vec![
            FilesystemGrant {
                provider: "data".into(),
                mount: "/data".into(),
                writable: true,
            },
            FilesystemGrant {
                provider: "data".into(),
                mount: "/view".into(),
                writable: false,
            },
        ];
        let mut orb = Orb::install(
            &engine,
            &artifact,
            install,
            "files::example:compiled-filesystem/probe@1.0.0",
        );
        let report = orb
            .call::<_, Reply<FilesystemReport>>("roundtrip", ("/data",))
            .unwrap()
            .value();
        assert_eq!(report.length, 150_006, "{strategy}");
        assert!(
            matches!(
                report.large_offset.as_str(),
                "eof" | "ErrorCode::Invalid" | "ErrorCode::Overflow"
            ),
            "{strategy}: native large file offset: {}",
            report.large_offset
        );
        assert_eq!(report.listing, ["renamed.bin"]);
        assert_eq!(report.directory_rename, "moved");
        // Deserialization itself verifies that both halves remain exact native u64 values.
        let _metadata_identity: (u64, u64) = (report.hash_lower, report.hash_upper);
        assert!(!directory.path().join("roundtrip").exists());
        let errors = orb
            .call::<_, Reply<FilesystemErrors>>("errors", ("/data", "/view"))
            .unwrap()
            .value();
        eprintln!("{strategy}: filesystem report {errors:?}");
        assert_eq!(errors.checked.len(), 15);
        #[cfg(target_os = "linux")]
        assert_eq!(
            errors.checked,
            [
                "missing:ErrorCode::NoEntry",
                "exclusive:ErrorCode::Exist",
                "traversal:../escape:ErrorCode::NotPermitted",
                "traversal:/escape:ErrorCode::NotPermitted",
                "traversal:child/../../escape:ErrorCode::NotPermitted",
                "nul:ErrorCode::Invalid",
                "wrong-kind:ErrorCode::NotDirectory",
                "readonly-file-size:ErrorCode::Invalid",
                "readonly-file-times:ErrorCode::NotPermitted",
                "read-directory-as-file:ErrorCode::IsDirectory",
                "read-file-as-directory:ErrorCode::NotDirectory",
                "readonly-directory:ErrorCode::NotPermitted",
                "readonly-open:ErrorCode::NotPermitted",
                "readlink-file:ErrorCode::Invalid",
                "readlink-missing:ErrorCode::NoEntry",
            ],
            "{strategy}: exact native platform error codes"
        );
        assert_eq!(
            errors
                .checked
                .iter()
                .filter(|item| item.starts_with("traversal:"))
                .count(),
            3,
            "each path escape is refused"
        );
        // Keep platform capability expectations Linux-scoped. Other operating systems may
        // require additional symlink privileges or expose different filesystem capabilities;
        // retain their observed outcomes without assuming these operations succeed.
        #[cfg(target_os = "linux")]
        {
            assert_eq!(errors.hard_link, "ok");
            assert_eq!(errors.symlink, "ok");
            assert_eq!(errors.timestamp, "ok");
            assert_eq!(errors.timestamp_at, "ok");
        }
        #[cfg(not(target_os = "linux"))]
        eprintln!(
            "{strategy}: platform-specific filesystem capabilities are not qualified on {}: hard-link={}, symlink={}, timestamp={}, timestamp-at={}",
            std::env::consts::OS,
            errors.hard_link,
            errors.symlink,
            errors.timestamp,
            errors.timestamp_at
        );
        assert_eq!(errors.timestamp_missing, "ok");
        #[cfg(target_os = "linux")]
        let baseline = descriptors_beneath(directory.path());
        orb.call::<_, Done>("retain", ("/data",)).unwrap().success();
        #[cfg(target_os = "linux")]
        assert!(
            descriptors_beneath(directory.path()) > baseline,
            "the stream physically retains the file after its descriptor drops"
        );
        assert_eq!(
            orb.call::<_, Reply<Vec<u8>>>("read-retained", EMPTY)
                .unwrap()
                .value(),
            b"seeded"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(descriptors_beneath(directory.path()), baseline);
        orb.call::<_, Done>("retain", ("/data",)).unwrap().success();
        orb.call::<_, ()>("drop-retained", EMPTY).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(descriptors_beneath(directory.path()), baseline);
        orb.call::<_, Done>("retain", ("/data",)).unwrap().success();
        assert!(orb.host.errors().is_empty());
        orb.close(&engine);
        #[cfg(target_os = "linux")]
        assert_eq!(
            descriptors_beneath(directory.path()),
            0,
            "Store retirement physically closes the unread stream and all preopens"
        );
        // The caller never drains the last stream; closing its Store still releases the file.
        std::fs::remove_file(directory.path().join("input.txt")).unwrap();
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpValueReport {
    checks: Vec<String>,
    duplicate_values: Vec<Vec<u8>>,
    exact_timeouts: (u64, u64, u64),
}

/// Synchronous HTTP resources are real component imports too: every accessor and mutator is
/// exercised without network I/O. Existing wasi_http tests cover requests and body lifecycle.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_http_resource_operations_preserve_exact_values() {
    let bytes = component("TEMBRA_WASM_HTTP_CLIENT");
    let artifact = artifact(&bytes, "http", "client", "web");
    for (strategy, engine) in engines() {
        let engine = engine.network("web").unwrap();
        let mut install = Install::default();
        install.grants = Grants {
            http: vec![HttpGrant {
                provider: "web".into(),
                origins: vec!["https://example.test".into()],
            }],
            ..Grants::default()
        };
        let mut orb = Orb::install(
            &engine,
            &artifact,
            install,
            "web::example:fetch/fetcher@1.0.0",
        );
        let report = orb
            .call::<_, Reply<HttpValueReport>>("values", EMPTY)
            .unwrap()
            .value();
        assert_eq!(
            report.checks,
            [
                "fields.new",
                "fields.get",
                "fields.has",
                "fields.duplicates",
                "fields.copy-all",
                "fields.set-and-clone",
                "fields.append",
                "fields.get-and-delete",
                "fields.delete",
                "fields.invalid",
                "fields.forbidden",
                "fields.limit",
                "fields.content-length",
                "options.defaults",
                "options.exact-and-clone",
                "options.reset",
                "request.defaults",
                "request.method",
                "request.scheme",
                "request.authority",
                "request.path",
                "request.immutable-headers",
                "request.immutable-options",
                "request.consume-body",
                "response.default-status",
                "response.status",
                "response.immutable-headers",
            ],
            "{strategy}: all operation groups executed"
        );
        assert_eq!(report.duplicate_values, [b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(
            report.exact_timeouts,
            ((1 << 53) + 1, u64::MAX - 1, u64::MAX)
        );
        assert!(orb.host.errors().is_empty());
        orb.close(&engine);
    }
}
