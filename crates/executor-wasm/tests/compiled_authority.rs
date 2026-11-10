//! Compiled admission trusts the binary, not artifact metadata (Phase 3A).
//!
//! The world below only *uses* the clock `instant` type, so its contract legitimately marks
//! `wasi:clocks/system-clock` as a type-only dependency and plans no clock import. The real
//! component built from it nevertheless calls `get-resolution`: it must need the clock grant.
use futures::executor::LocalPool;
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Grants, Install, Limits, SystemCapability};

const WIT: &str = r#"package example:stamp@1.0.0;
interface stamps { use wasi:clocks/system-clock@0.3.0.{instant}; ticks: func() -> u32; }
world app { export stamps; }"#;

const MODULE: &str = r#"(module
  (import "wasi:clocks/system-clock@0.3.0" "get-resolution" (func $resolution (result i64)))
  (memory (export "memory") 1)
  (func (export "example:stamp/stamps@1.0.0#ticks") (result i32) call $resolution i32.wrap_i64))"#;

fn artifact() -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str(
            "clocks.wit",
            include_str!("../../contracts/wit/wasi03/clocks.wit"),
        )
        .unwrap();
    let package = resolve.push_str("stamp.wit", WIT).unwrap();
    let world = resolve.select_world(&[package], Some("app")).unwrap();
    let mut core = wat::parse_str(MODULE).unwrap();
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .unwrap()
        .encode()
        .unwrap();
    let contract = Contract::from_source("stamp.wit", WIT, Some("app")).unwrap();
    let description = contract.describe();
    let clock = description["imports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["contract"] == "wasi:clocks/system-clock@0.3.0")
        .unwrap();
    assert_eq!(
        clock["type_only"], true,
        "metadata marks the clock as a dependency"
    );
    contract
        .component_artifact(&component, "stamp", 1, &Default::default())
        .unwrap()
}

fn install(clocks: bool) -> tembra::Result<()> {
    let engine = Engine::new(Limits::default()).unwrap();
    let mut install = Install::default();
    if clocks {
        install.grants = Grants {
            system: [SystemCapability::Clocks].into(),
            ..Grants::default()
        };
    }
    let orb = engine.load(&artifact(), install)?;
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb])
}

#[test]
fn a_binary_that_calls_the_clock_needs_the_grant_whatever_its_metadata_says() {
    let refused = install(false).unwrap_err();
    assert!(refused.to_string().contains("system-clock"), "{refused}");
    install(true).unwrap();
}

/// The same type-only clock trick, in a world that honestly imports `wasi:cli/stdout` and
/// `wasi:random/insecure-seed`, so the CLI and Random grants match its planned imports.
const SIDE_EFFECT_WIT: &str = r#"package example:stamp@1.0.0;
interface stamps { use wasi:clocks/system-clock@0.3.0.{instant}; ticks: func() -> u32; }
world app {
  import wasi:cli/stdout@0.3.0;
  import wasi:random/insecure-seed@0.3.0;
  export stamps;
}"#;

fn side_effect_artifact() -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    for (name, wit) in [
        (
            "clocks.wit",
            include_str!("../../contracts/wit/wasi03/clocks.wit"),
        ),
        (
            "random.wit",
            include_str!("../../contracts/wit/wasi03/random.wit"),
        ),
        (
            "filesystem.wit",
            include_str!("../../contracts/wit/wasi03/filesystem.wit"),
        ),
        (
            "sockets.wit",
            include_str!("../../contracts/wit/wasi03/sockets.wit"),
        ),
        (
            "cli.wit",
            include_str!("../../contracts/wit/wasi03/cli.wit"),
        ),
    ] {
        resolve.push_str(name, wit).unwrap();
    }
    let package = resolve.push_str("stamp.wit", SIDE_EFFECT_WIT).unwrap();
    let world = resolve.select_world(&[package], Some("app")).unwrap();
    let mut core = wat::parse_str(MODULE).unwrap();
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .unwrap()
        .encode()
        .unwrap();
    let contract = Contract::from_source("stamp.wit", SIDE_EFFECT_WIT, Some("app")).unwrap();
    contract
        .component_artifact(&component, "stamp", 1, &Default::default())
        .unwrap()
}

/// A binary refused at admission opens no configured CLI file and invokes no configured
/// clock or seed factory: admission reads the actual binary before any provider is prepared.
#[test]
fn a_refused_binary_creates_no_cli_file_and_invokes_no_factory() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tembra_executor_wasm::{Cli, Output};
    struct Wall;
    impl tembra_executor_wasm::HostWallClock for Wall {
        fn resolution(&self) -> Duration {
            Duration::from_nanos(1)
        }
        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }
    struct Monotonic;
    impl tembra_executor_wasm::HostMonotonicClock for Monotonic {
        fn resolution(&self) -> u64 {
            1
        }
        fn now(&self) -> u64 {
            0
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (wall, monotonic, seed) = (calls.clone(), calls.clone(), calls.clone());
    let engine = Engine::new(Limits::default())
        .unwrap()
        .wall_clock(move || {
            wall.fetch_add(1, Ordering::SeqCst);
            Wall
        })
        .monotonic_clock(move || {
            monotonic.fetch_add(1, Ordering::SeqCst);
            Monotonic
        })
        .insecure_random_seed(move || {
            seed.fetch_add(1, Ordering::SeqCst);
            7
        });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("out.bin");
    let artifact = side_effect_artifact();
    let install = |system: &[SystemCapability]| -> tembra::Result<()> {
        let mut install = Install::default();
        install.grants = Grants {
            system: system.iter().copied().collect(),
            ..Grants::default()
        };
        let install = install.cli(Cli {
            stdout: Output::File(path.clone()),
            ..Cli::default()
        });
        let orb = engine.load(&artifact, install)?;
        let pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb])
    };

    // Every planned import is granted, but the binary's actual clock import is not.
    let refused = install(&[SystemCapability::Cli, SystemCapability::Random]).unwrap_err();
    assert!(refused.to_string().contains("system-clock"), "{refused}");
    assert!(
        !path.exists(),
        "a refused installation creates no stdout file"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no configured factory ran");

    // The same configuration with the clock grant really touches both.
    install(&[
        SystemCapability::Cli,
        SystemCapability::Random,
        SystemCapability::Clocks,
    ])
    .unwrap();
    assert!(path.is_file());
    assert_eq!(calls.load(Ordering::SeqCst), 3, "wall, monotonic and seed");
}
