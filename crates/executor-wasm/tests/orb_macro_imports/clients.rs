//! The generated service client uses one API for native and compiled WIT implementations.
use super::{CODEC, compiled, exporter};
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

#[test]
#[ignore = "requires the codec component built by just build-import-resources"]
fn generated_service_clients_call_native_and_compiled_providers() {
    let engine = Engine::new(Limits::default()).unwrap();
    for native in [true, false] {
        let provider = if native {
            exporter::orb_named("codec", 1).unwrap()
        } else {
            compiled(
                &engine,
                "TEMBRA_WASM_CODEC_PROVIDER",
                "codec",
                Install::default(),
            )
        };
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![provider]).unwrap();
        let reference = Rc::new(
            host.get(&format!("codec::{CODEC}"), vec!["main".into()], "host")
                .unwrap(),
        );
        let client = exporter::implementation::CodecClient::new(reference.clone());
        pool.run_until(host.root("host", move |context| {
            async move {
                assert_eq!(client.normalize(&context, 41).await?, 42);
                assert_eq!(
                    client
                        .echo(
                            &context,
                            (Some("model".into()), Ok(vec!["a".into(), "b".into()]))
                        )
                        .await?,
                    (Some("model".into()), Ok(vec!["a".into(), "b".into()]))
                );
                assert_eq!(
                    client.echo(&context, (None, Err("domain".into()))).await?,
                    (None, Err("domain".into()))
                );
                let before = client.dropped(&context).await?;
                let decoder = client.new_decoder(&context, 5).await?;
                assert_eq!(decoder.total(&context).await?, 5);
                assert_eq!(decoder.next(&context).await?, Some(6));
                assert_eq!(decoder.total(&context).await?, 6);
                drop(decoder);
                assert_eq!(client.dropped(&context).await?, before + 1);
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(host.kernel().diagnostics().references, 0);
        assert_eq!(host.kernel().diagnostics().calls, 0);
    }
}
