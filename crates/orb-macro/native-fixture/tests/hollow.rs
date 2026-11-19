//! `hollow::orb()` in a native host: a service in a private module, whose state sits in a module
//! below it and whose WIT resource sits in another, reached through the client and the handle the
//! Orb re-exports.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_orb_native_fixture::hollow::{self, TokenHandle, TokensClient};

const TOKENS: &str = "calculator::example:hollow/tokens@1.0.0";

/// The service's methods, the resource's constructor and static function through the client, and
/// the resource's method through its handle each reach what belongs to the service: the
/// constructor counts in the state of the object issuing the token, which the kernel keeps.
#[test]
fn a_service_reaches_its_state_and_resource_in_modules_below_it() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![hollow::orb().unwrap()]).unwrap();
    let reference = Rc::new(host.get(TOKENS, vec!["main".into()], "host").unwrap());
    let state = reference.state::<Value>().unwrap();
    let client = TokensClient::new(reference.clone());
    let numbers = pool
        .run_until(host.root("host", move |context| {
            async move {
                let built: TokenHandle = client.new_token(&context, 3).await?;
                let tenfold: TokenHandle = client.token_tenfold(&context, 2).await?;
                let opened: TokenHandle = client.open(&context, 4).await?;
                let mut numbers = Vec::new();
                for token in [&built, &tenfold, &opened] {
                    numbers.push(token.number(&context).await?);
                }
                numbers.push(client.issued(&context).await?);
                Ok(numbers)
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(numbers, [3, 20, 4, 3]);
    assert_eq!(*state.value().unwrap(), json!({"count": 3}));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
