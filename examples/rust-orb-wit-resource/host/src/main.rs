//! The wiring: install the compiled `text` Orb and the compiled `speller`, which imports the text
//! Orb's interface, drive the speller, then hold two builders from the host itself. No Orb is
//! defined here.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::wasm::{Engine, Install, Limits};
use tembra::{Host, Reference, portable::Owned};

/// The text Orb's interface, imported by the speller and bound to the `text` Orb at installation.
const TEXTS: &str = "example:text/texts@1.0.0";
const TEXT: &str = "text::example:text/texts@1.0.0";
const SPELLER: &str = "speller::example:speller/spelling@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let (Some(text), Some(speller)) = (arguments.next(), arguments.next()) else {
        return Err("expected text.orb.wasm and speller.orb.wasm".into());
    };
    let engine = Engine::new(Limits::default())?;
    // 1. The text Orb exports its builders; the speller imports them, bound here to `text`.
    let text = engine.load(&std::fs::read(text)?, Install::default())?;
    let speller = engine.load(
        &std::fs::read(speller)?,
        Install::default().requires(TEXTS, "text", 1),
    )?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![text, speller])?;
    let text = Rc::new(host.get(TEXT, vec!["main".into()], "alice")?);
    let speller = Rc::new(host.get(SPELLER, vec!["main".into()], "alice")?);
    pool.run_until(text.ready())?;
    pool.run_until(speller.ready())?;

    // 2. The speller starts one builder in the text Orb, pushes seven characters through the
    //    handle, reads the text and drops it: the text Orb counts one destroyed builder.
    let spelled = call(&mut pool, &host, &speller, "spell", json!(["ciao"]))?;
    println!("speller spell(\"ciao\") -> {spelled}");
    println!(
        "text    dropped()     -> {}",
        call(&mut pool, &host, &text, "dropped", json!([]))?
    );

    // 3. The host holds two builders at once: two `Owned` handles, called through its own
    //    admission, independent of each other, destroyed when dropped.
    let held = text.clone();
    let (first, second) = pool.run_until(host.root("alice", move |context| {
        async move {
            let first = held
                .call_portable::<_, Owned>(&context, "start", json!([]))
                .await?;
            let second = held
                .call_portable::<_, Owned>(&context, "start", json!([]))
                .await?;
            for (a, b) in [('h', 'y'), ('i', 'o')] {
                first
                    .call_portable::<_, Value>(&context, "[method]builder.push", json!([a]))
                    .await?;
                second
                    .call_portable::<_, Value>(&context, "[method]builder.push", json!([b]))
                    .await?;
            }
            let texts = (
                first
                    .call_portable::<_, Value>(&context, "[method]builder.text", json!([]))
                    .await?,
                second
                    .call_portable::<_, Value>(&context, "[method]builder.text", json!([]))
                    .await?,
            );
            // Both handles are dropped here, at the end of the root.
            Ok(texts)
        }
        .boxed_local()
    }))?;
    println!("host    two builders  -> {first}, {second}");
    pool.run_until(host.settled());
    println!(
        "text    dropped()     -> {}",
        call(&mut pool, &host, &text, "dropped", json!([]))?
    );
    text.release()?;
    speller.release()?;
    pool.run_until(host.close())?;
    // Cleanup failures are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// Call one method under a root context of the principal `alice`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        Box::pin(async move { reference.call_portable(&context, method, arguments).await })
    }))
}
