//! `wasi:cli/exit` in a compiled Orb is contained: the host keeps running, the invoking
//! revision fails with the exact status, and nothing reports success (Phase 3B).
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Grants, Install, Limits, SystemCapability};

const WIT: &str = r#"package example:quit;
interface run { go: func() -> u32; }
world app { import wasi:cli/exit@0.3.0; export run; }"#;

const MODULE: &str = r#"(module
  (import "wasi:cli/exit@0.3.0" "exit-with-code" (func $exit (param i32)))
  (memory (export "memory") 1)
  (func (export "example:quit/run#go") (result i32) i32.const 7 call $exit i32.const 0))"#;

fn artifact() -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    for (name, source) in [
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
        resolve.push_str(name, source).unwrap();
    }
    let package = resolve.push_str("quit.wit", WIT).unwrap();
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
    Contract::from_source("quit.wit", WIT, Some("app"))
        .unwrap()
        .component_artifact(&component, "quit", 1, &Default::default())
        .unwrap()
}

#[test]
fn a_compiled_exit_fails_only_its_revision_with_the_exact_status() {
    let engine = Engine::new(Limits::default()).unwrap();
    // Without the CLI grant the import is refused at installation.
    assert!(engine.load(&artifact(), Install::default()).is_err());
    let mut install = Install::default();
    install.grants = Grants {
        system: [SystemCapability::Cli].into(),
        ..Grants::default()
    };
    let orb = engine.load(&artifact(), install).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let reference = Rc::new(
        host.get("quit::example:quit/run", vec!["main".into()], "test")
            .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let call = reference.clone();
    let outcome = pool.run_until(host.root("test", move |context| {
        async move { call.call::<_, Value>(&context, "go", json!([])).await }.boxed_local()
    }));
    let error = outcome.unwrap_err().to_string();
    assert!(
        error.contains("orb exited with status 7; component retired"),
        "{error}"
    );
    // The host and its runtime keep working; the exited revision is closed normally.
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
