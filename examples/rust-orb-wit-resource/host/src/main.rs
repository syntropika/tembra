//! The wiring: install the compiled `decoder` and the compiled `player`, which imports the
//! decoder's interface, drive the player, then hold a session from the host itself. No Orb is
//! defined here.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference, portable::Owned};
use tembra_executor_wasm::{Engine, Install, Limits};

/// The decoder's interface, imported by the player and bound to the `decoder` Orb at installation.
const DECODING: &str = "example:decoder/decoding@1.0.0";
const DECODER: &str = "decoder::example:decoder/decoding@1.0.0";
const PLAYER: &str = "player::example:player/playback@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let (Some(decoder), Some(player)) = (arguments.next(), arguments.next()) else {
        return Err("expected decoder.orb.wasm and player.orb.wasm".into());
    };
    let engine = Engine::new(Limits::default())?;
    // 1. The decoder exports its sessions; the player imports them, bound here to `decoder`.
    let decoder = engine.load(&std::fs::read(decoder)?, Install::default())?;
    let player = engine.load(
        &std::fs::read(player)?,
        Install::default().requires(DECODING, "decoder", 1),
    )?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![decoder, player])?;
    let decoder = Rc::new(host.get(DECODER, vec!["main".into()], "alice")?);
    let player = Rc::new(host.get(PLAYER, vec!["main".into()], "alice")?);
    pool.run_until(decoder.ready())?;
    pool.run_until(player.ready())?;

    // 2. The player opens one session in the decoder, decodes three frames through the handle and
    //    drops it: the decoder counts one destroyed session.
    let played = call(&mut pool, &host, &player, "play", json!([7, [[7, 7], [14], [0, 7]]]))?;
    println!("player play(7, 3 frames) -> {played}");
    println!(
        "decoder closed()          -> {}",
        call(&mut pool, &host, &decoder, "closed", json!([]))?
    );

    // 3. The host holds a session too: an `Owned` handle, called through its own admission, and
    //    destroyed when dropped.
    let held = decoder.clone();
    let (first, second) = pool.run_until(host.root("alice", move |context| {
        async move {
            let session = held
                .call_portable::<_, Owned>(&context, "open", json!([1]))
                .await?;
            let first = session
                .call_portable::<_, Value>(&context, "[method]session.decode", json!([[3, 2]]))
                .await?;
            let second = session
                .call_portable::<_, Value>(&context, "[method]session.frames", json!([]))
                .await?;
            // The handle is dropped here, at the end of the root.
            Ok((first, second))
        }
        .boxed_local()
    }))?;
    println!("host   decode([3, 2])     -> {first}, frames -> {second}");
    pool.run_until(host.settled());
    println!(
        "decoder closed()          -> {}",
        call(&mut pool, &host, &decoder, "closed", json!([]))?
    );
    decoder.release()?;
    player.release()?;
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
