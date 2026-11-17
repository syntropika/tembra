//! `keeper::orb()` in a native host: the Orb implements an interface of the package in `wit/`,
//! whose record, variant, enums and flags are the types the bindings generate from that WIT. A
//! local caller passes them as they are; a portable caller sends and receives the boundary's JSON,
//! which the native codecs read and write from the interface's WIT, and which the module's contract
//! accepts.
use futures::{FutureExt, executor::LocalPool, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::{path::Path, rc::Rc};
use tembra::{Context, Host, Reference, contracts::Contract};
use tembra_orb_native_fixture::keeper::{self, book};

const BOOK: &str = "calculator::example:ledger/book@1.0.0";
const INTERFACE: &str = "example:ledger/book@1.0.0";

/// The module's contract: its `WIT`, read with the packages of `wit/`, as a compiled or
/// TypeScript caller validates what it sends to the Orb and what it receives.
fn contract() -> Contract {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("wit");
    Contract::from_source_in(dir, "keeper.wit", keeper::WIT, Some("keeper")).unwrap()
}

/// Whether the contract accepts `value` as the arguments, or as the result, of a method of the
/// book.
fn accepted(contract: &Contract, method: &str, arguments: bool, value: &Value) -> bool {
    let slot = contract.bind(INTERFACE, method, true).unwrap();
    contract.validate_slot(slot, arguments, value).is_ok()
}

/// Install the Orb in a host and run `test` under a root of the principal `host`, with a
/// reference to the book's `main` object.
fn with_book(
    test: impl FnOnce(Rc<Reference>, Context) -> LocalBoxFuture<'static, tembra::Result<()>>,
) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![keeper::orb().unwrap()]).unwrap();
    let reference = Rc::new(host.get(BOOK, vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    pool.run_until(host.root("host", move |context| test(called, context)))
        .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// An entry of the book, in euros, with no marks.
fn entry(label: &str, amount: u64, direction: book::Direction) -> book::Entry {
    book::Entry {
        label: label.to_string(),
        amount,
        currency: book::Currency::Eur,
        direction,
        marks: book::Marks::empty(),
    }
}

/// `orb()` declares what the kernel computes for a component of the same contract: the book's
/// service with the signature the interface's WIT gives it.
#[test]
fn the_declaration_is_the_plan_of_the_contract() {
    let expected = contract()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    assert_eq!(keeper::orb().unwrap().declaration(), &expected);
    assert!(expected.services[0].signature.is_some());
}

/// A local caller passes and gets the bindings' types as they are, through the client too: the
/// record, its enums, `use`d from another interface or not, and its flags, the variant, a list of
/// records and the interface's alias of one, `book::Journal`.
#[test]
fn local_calls_move_the_bindings_types() {
    with_book(|book, context| {
        async move {
            let outcome = book
                .call::<_, book::Outcome>(
                    &context,
                    "add",
                    (entry("rent", 700, book::Direction::Debit),),
                )
                .await?;
            assert!(matches!(outcome, book::Outcome::Recorded(-700)), "{outcome:?}");
            let client = keeper::KeeperClient::new(book.clone());
            let mut pay = entry("pay", 1000, book::Direction::Credit);
            pay.currency = book::Currency::Usd;
            pay.marks = book::Marks::AUDITED | book::Marks::DISPUTED_ONCE;
            let outcome = client.add(&context, pay).await?;
            assert!(matches!(outcome, book::Outcome::Recorded(300)), "{outcome:?}");
            let outcome = client
                .add(&context, entry("nothing", 0, book::Direction::Credit))
                .await?;
            assert!(
                matches!(&outcome, book::Outcome::Refused(reason) if reason == "`nothing` moves no amount"),
                "{outcome:?}"
            );
            let credits = client
                .entries(&context, Some(book::Direction::Credit))
                .await?;
            let [pay] = credits.as_slice() else {
                panic!("{credits:?}");
            };
            assert_eq!(
                (pay.label.as_str(), pay.currency, pay.marks),
                (
                    "pay",
                    book::Currency::Usd,
                    book::Marks::AUDITED | book::Marks::DISPUTED_ONCE
                )
            );
            assert_eq!(client.entries(&context, None).await?.len(), 2);
            let recent: book::Journal = client.recent(&context, 1).await?;
            let labels: Vec<&str> = recent.iter().map(|entry| entry.label.as_str()).collect();
            assert_eq!(labels, ["pay"]);
            let recent = book
                .call::<_, book::Journal>(&context, "recent", (5u32,))
                .await?;
            let labels: Vec<&str> = recent.iter().map(|entry| entry.label.as_str()).collect();
            assert_eq!(labels, ["rent", "pay"]);
            Ok(())
        }
        .boxed_local()
    });
}

/// A portable caller sends and receives the boundary's JSON, which the contract accepts both
/// ways: the record as an object of lowerCamelCase fields, an enum as the WIT name of its case,
/// `use`d from another interface or not, the flags as an object of the flags set, which reads a
/// flag left out, or `false`, as unset, the variant as `{tag, val}`, an option as `null` or the
/// value, a list as an array, and the interface's alias of a list as the list.
#[test]
fn portable_calls_write_what_the_contract_accepts() {
    with_book(|book, context| {
        async move {
            let contract = contract();
            let call = |method: &'static str, arguments: Value| {
                let book = book.clone();
                let context = context.clone();
                let contract = &contract;
                async move {
                    assert!(
                        accepted(contract, method, true, &arguments),
                        "{method}: {arguments}"
                    );
                    let answer = book
                        .call_portable::<_, Value>(&context, method, arguments)
                        .await?;
                    assert!(
                        accepted(contract, method, false, &answer),
                        "{method}: {answer}"
                    );
                    tembra::Result::Ok(answer)
                }
            };
            let rent = json!({
                "label": "rent",
                "amount": 700,
                "currency": "eur",
                "direction": "debit",
                "marks": { "audited": true }
            });
            assert_eq!(
                call("add", json!([rent])).await?,
                json!({ "tag": "recorded", "val": -700 })
            );
            assert_eq!(
                call(
                    "add",
                    json!([{
                        "label": "pay",
                        "amount": 1000,
                        "currency": "usd",
                        "direction": "credit",
                        "marks": { "audited": false, "disputedOnce": true }
                    }])
                )
                .await?,
                json!({ "tag": "recorded", "val": 300 })
            );
            assert_eq!(
                call(
                    "add",
                    json!([{
                        "label": "nothing",
                        "amount": 0,
                        "currency": "eur",
                        "direction": "credit",
                        "marks": {}
                    }])
                )
                .await?,
                json!({ "tag": "refused", "val": "`nothing` moves no amount" })
            );
            assert_eq!(call("entries", json!(["debit"])).await?, json!([rent]));
            let pay = json!({
                "label": "pay",
                "amount": 1000,
                "currency": "usd",
                "direction": "credit",
                "marks": { "disputedOnce": true }
            });
            assert_eq!(call("entries", json!([null])).await?, json!([rent, pay]));
            // The interface's alias of a list of entries reads as the list.
            assert_eq!(call("recent", json!([1])).await?, json!([pay]));
            assert_eq!(call("recent", json!([0])).await?, json!([]));
            Ok(())
        }
        .boxed_local()
    });
}

/// What the boundary's JSON does not spell fails the call as its arguments, before the method
/// runs, and the contract refuses it too: a record with a field missing, an enum case the WIT does
/// not name, of the book's enum or of the one it `use`s, and a flag the WIT does not name.
#[test]
fn what_the_contract_refuses_fails_as_arguments() {
    with_book(|book, context| {
        async move {
            let contract = contract();
            let entry = |field: &str, value: Value| {
                let mut entry = json!({
                    "label": "rent",
                    "amount": 700,
                    "currency": "eur",
                    "direction": "debit",
                    "marks": {}
                });
                entry[field] = value;
                json!([entry])
            };
            let mut missing = entry("label", json!("rent"));
            missing[0].as_object_mut().unwrap().remove("amount");
            for (arguments, message) in [
                (missing, "missing the field `amount`"),
                (entry("currency", json!("gbp")), "unknown case `gbp`"),
                (entry("direction", json!("Debit")), "unknown case `Debit`"),
                (
                    entry("marks", json!({ "lost": true })),
                    "unknown flag `lost`",
                ),
            ] {
                assert!(!accepted(&contract, "add", true, &arguments), "{arguments}");
                let error = book
                    .call_portable::<_, Value>(&context, "add", arguments)
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "arguments", "{error}");
                assert_eq!(error.message, message);
            }
            let error = book
                .call_portable::<_, Value>(&context, "entries", json!(["sideways"]))
                .await
                .unwrap_err();
            assert_eq!(
                (error.reason.as_str(), error.message.as_str()),
                ("arguments", "unknown case `sideways`")
            );
            Ok(())
        }
        .boxed_local()
    });
}
