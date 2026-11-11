//! Prepared SDK source fixtures from `tests/source`, built into `TEMBRA_WASM_FIXTURES`.
#![allow(dead_code)]
use futures::{FutureExt, executor::LocalPool};
use serde_json::Value;
use std::rc::Rc;
use tembra::{Error, Host, Reference};
use tembra_executor_wasm::{Engine, Install, Limits};

/// Read one prepared artifact by fixture directory name.
pub fn fixture(name: &str) -> Vec<u8> {
    let directory = std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES");
    std::fs::read(std::path::Path::new(&directory).join(format!("{name}.wasm")))
        .unwrap_or_else(|error| panic!("{name}.wasm: {error}"))
}

/// Native code (`false`) or the portable Pulley interpreter (`true`).
pub fn engine(interpreted: bool) -> Engine {
    if interpreted {
        Engine::interpreted(Limits::default()).unwrap()
    } else {
        Engine::new(Limits::default()).unwrap()
    }
}

/// Native code and the portable Pulley interpreter execute the same artifact.
pub fn engines() -> [Engine; 2] {
    [engine(false), engine(true)]
}

/// One installed host with references to services by their kernel names.
pub struct Installed {
    pub pool: LocalPool,
    pub host: Host,
}

impl Installed {
    /// Install artifacts in order with their installation options.
    pub fn new(engine: &Engine, orbs: Vec<(Vec<u8>, Install)>) -> Self {
        let pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let orbs = orbs
            .into_iter()
            .map(|(bytes, install)| engine.load(&bytes, install).unwrap())
            .collect();
        host.install(orbs).unwrap();
        Self { pool, host }
    }

    /// Acquire a ready `main` reference as `principal`.
    pub fn reference(&mut self, service: &str, principal: &str) -> Rc<Reference> {
        let reference = Rc::new(
            self.host
                .get(service, vec!["main".into()], principal)
                .unwrap(),
        );
        self.pool.run_until(reference.ready()).unwrap();
        reference
    }

    /// Call one method as `principal` in its own root and return the portable outcome.
    pub fn call(
        &mut self,
        reference: &Rc<Reference>,
        principal: &str,
        method: &'static str,
        args: Value,
    ) -> Result<Value, Error> {
        let reference = reference.clone();
        self.pool
            .run_until(self.host.root(principal, move |context| {
                async move {
                    reference
                        .call_portable::<_, Value>(&context, method, args)
                        .await
                }
                .boxed_local()
            }))
    }

    /// Close the host and require that nothing remains live.
    pub fn close(mut self, engine: &Engine) {
        self.pool.run_until(self.host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert_eq!(engine.resources(), 0);
    }

    /// Close a host whose revision was quarantined: closing reports the trap, and the Store is
    /// still physically retired.
    pub fn retire(mut self, engine: &Engine) -> String {
        let closed = self.pool.run_until(self.host.close());
        assert_eq!(engine.instances(), 0);
        assert_eq!(engine.resources(), 0);
        closed.err().map(|error| error.message).unwrap_or_default()
    }
}
