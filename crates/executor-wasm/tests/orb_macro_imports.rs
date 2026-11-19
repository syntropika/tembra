//! Actual macro-generated decoders and consumers across native and compiled providers.
use futures::{FutureExt, executor::LocalPool};
use std::{path::Path, rc::Rc};
use tembra::{Host, Orb, Provider};
use tembra_contracts::{EmbeddedManifest, ModuleContract};
use tembra_executor_wasm::{Engine, Install, Limits};
use tembra_orb_native_fixture::codec::{exporter, importer};

const CODEC: &str = "example:protocol/codec@1.0.0";

/// Package the real macro component with its fixed WIT and embedded roles.
pub(crate) fn compiled(engine: &Engine, variable: &str, name: &str, install: Install) -> Orb {
    let path = std::env::var_os(variable).unwrap_or_else(|| panic!("{variable} is required"));
    let bytes = std::fs::read(path).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/runtime-fixture");
    let consumed = EmbeddedManifest::read(&bytes)
        .unwrap()
        .wit
        .map(|dir| fixture.join(dir));
    let module = ModuleContract::from_component(&bytes, consumed.as_deref())
        .unwrap()
        .unwrap();
    let contract = module.contract();
    let roles = contract.embedded_roles(&bytes).unwrap();
    let artifact = contract
        .component_artifact(&bytes, name, 1, &roles)
        .unwrap();
    engine.load(&artifact, install).unwrap()
}

#[path = "orb_macro_imports/awaitable.rs"]
mod awaitable;

/// Each combination must preserve synchronous contracts, identity, suspension and ownership.
#[test]
#[ignore = "requires the codec components built by just build-import-resources"]
fn generated_consumers_and_fixed_wit_providers_compose_both_ways() {
    let engine = Engine::new(Limits::default()).unwrap();
    for native_provider in [true, false] {
        for native_consumer in [true, false] {
            let provider = if native_provider {
                exporter::orb_named("codec", 1).unwrap()
            } else {
                compiled(
                    &engine,
                    "TEMBRA_WASM_CODEC_PROVIDER",
                    "codec",
                    Install::default(),
                )
            };
            let consumer = if native_consumer {
                importer::orb_named(
                    "pipeline",
                    1,
                    importer::Providers {
                        codec: Provider::new("codec", 1),
                    },
                )
                .unwrap()
            } else {
                compiled(
                    &engine,
                    "TEMBRA_WASM_CODEC_CONSUMER",
                    "pipeline",
                    Install::default().requires(CODEC, "codec", 1),
                )
            };
            let mut pool = LocalPool::new();
            let host = Host::new(pool.spawner());
            host.install(vec![provider, consumer]).unwrap();
            let reference = Rc::new(
                host.get(
                    "pipeline::example:codec-consumer/pipeline@1.0.0",
                    vec!["main".into()],
                    "host",
                )
                .unwrap(),
            );
            pool.run_until(reference.ready()).unwrap();
            let claims = host.kernel().diagnostics().references;
            let called = reference.clone();
            let before = exporter::implementation::Codec::dropped();
            let result = pool
                .run_until(host.root("host", move |context| {
                    async move {
                        called
                            .call_portable::<_, (u64, u32)>(&context, "run", (10_u64,))
                            .await
                    }
                    .boxed_local()
                }))
                .unwrap();
            assert_eq!(
                result,
                (51, 0),
                "native provider: {native_provider}, native consumer: {native_consumer}"
            );
            assert_eq!(host.kernel().diagnostics().references, claims);
            let codec = Rc::new(
                host.get(
                    "codec::example:protocol/codec@1.0.0",
                    vec!["main".into()],
                    "host",
                )
                .unwrap(),
            );
            let called = codec.clone();
            let dropped = pool
                .run_until(host.root("host", move |context| {
                    async move {
                        called
                            .call_portable::<_, u32>(&context, "dropped", Vec::<u32>::new())
                            .await
                    }
                    .boxed_local()
                }))
                .unwrap();
            assert_eq!(dropped, if native_provider { before + 5 } else { 5 });
            codec.release().unwrap();
            reference.release().unwrap();
            pool.run_until(host.close()).unwrap();
            assert!(host.errors().is_empty(), "{:?}", host.errors());
        }
    }
}
