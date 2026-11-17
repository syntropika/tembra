//! `list::orb()` in a native host: the module, a method and a flag are named like keywords of WIT,
//! which the WIT writes with its `%` escape, and the Orb answers under those names.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{
    Host,
    contracts::{Contract, ModuleContract},
};
use tembra_orb_native_fixture::list::{self, Marks};

const WORDS: &str = "calculator::example:keywords/words@1.0.0";

/// The module `list` is the world `%list`, whose contract is `contract/%list.wit`, the method
/// `record` is `%record` and the flag `r#type` is `%type`; the Orb declares what the plan of that
/// world computes.
#[test]
fn names_that_are_wit_keywords_are_escaped() {
    let words: String = list::WIT.split_whitespace().collect();
    for expected in [
        "world%list{exportwords;}",
        "flagsmarks{bold,%type,}",
        "%record:func(word:string,marks:marks)->marks;",
    ] {
        assert!(words.contains(expected), "{expected}\n{}", list::WIT);
    }
    assert_eq!(
        ModuleContract::new(list::WIT, None).unwrap().world(),
        "%list"
    );
    let expected = Contract::from_source("%list.wit", list::WIT, Some("%list"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    assert_eq!(list::orb().unwrap().declaration(), &expected);
}

/// The method `record` answers under its WIT name, through the client and portably, and the flag
/// `r#type` crosses both ways, `type` in the boundary's JSON.
#[test]
fn a_method_named_record_answers() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![list::orb().unwrap()]).unwrap();
    let words = Rc::new(host.get(WORDS, vec!["main".into()], "alice").unwrap());
    let client = list::WordsClient::new(words.clone());
    let called = words.clone();
    let (local, portable) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let marks = Marks {
                    bold: true,
                    r#type: false,
                };
                let local = client.record(&context, "List".into(), marks).await?;
                let portable = called
                    .call_portable::<_, Value>(&context, "record", json!(["list", {"type": true}]))
                    .await?;
                Ok((local, portable))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!((local.bold, local.r#type), (true, true));
    assert_eq!(portable, json!({"type": true}));
    words.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
