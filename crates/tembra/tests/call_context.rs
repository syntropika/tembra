//! A native method reads the address of the object running its call, and writes a record for the
//! host's log observer, attributed to its Orb by the host. Without an observer the record is
//! dropped, a later observer replaces an earlier one, and a root, which runs no call, has neither.
use futures::{FutureExt, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, LogRecord, Orb, Service};

/// The records an observer saw.
type Records = Rc<RefCell<Vec<LogRecord>>>;

/// An observer that keeps the records it sees in `records`.
fn keep(records: &Records) -> impl Fn(&LogRecord) + 'static {
    let records = records.clone();
    move |record| records.borrow_mut().push(record.clone())
}

/// A journal whose `write` logs its text and answers the key and principal of its object.
fn journal() -> Orb {
    Orb::new("notes", 1).service(Service::new("notes::journal", |_| Ok(())).method(
        "write",
        |_, context, (text,): (String,)| async move {
            context.log("info", text)?;
            let address = context.address()?;
            Ok((address.key, address.principal))
        },
    ))
}

/// Install the journal in a host `observe` sets up, and write `text` to the object `room/7` of
/// `alice`.
fn write(observe: impl FnOnce(&Host), text: &str) -> (Vec<String>, String) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    observe(&host);
    host.install(vec![journal()]).unwrap();
    let key = vec!["room".to_owned(), "7".to_owned()];
    let reference = Rc::new(host.get("notes::journal", key, "alice").unwrap());
    let called = reference.clone();
    let text = text.to_owned();
    let written = pool
        .run_until(host.root("alice", move |context| {
            async move {
                // A root runs no call: no object answers it and no Orb writes from it.
                assert_eq!(context.address().unwrap_err().reason, "context");
                assert_eq!(context.log("info", "lost").unwrap_err().reason, "context");
                called
                    .call::<_, (Vec<String>, String)>(&context, "write", (text,))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    written
}

/// The records an observer saw, as `(orb, level, message)`.
fn logged(records: &Records) -> Vec<(String, String, String)> {
    records
        .borrow()
        .iter()
        .map(|record| {
            (
                record.orb.clone(),
                record.level.clone(),
                record.message.clone(),
            )
        })
        .collect()
}

#[test]
fn a_call_reads_its_object_and_logs_for_the_observer() {
    let records = Records::default();
    let (key, principal) = write(|host| host.on_log(keep(&records)), "hello");
    assert_eq!(
        (key.join("/"), principal.as_str()),
        ("room/7".to_owned(), "alice")
    );
    assert_eq!(
        logged(&records),
        [("notes".to_owned(), "info".to_owned(), "hello".to_owned())]
    );
}

#[test]
fn without_an_observer_a_record_is_dropped() {
    let (key, _) = write(|_| {}, "unheard");
    assert_eq!(key.join("/"), "room/7");
}

#[test]
fn a_later_observer_replaces_the_earlier_one() {
    let (earlier, later) = (Records::default(), Records::default());
    write(
        |host| {
            host.on_log(keep(&earlier));
            host.on_log(keep(&later));
        },
        "once",
    );
    assert!(earlier.borrow().is_empty(), "{:?}", earlier.borrow());
    assert_eq!(
        logged(&later),
        [("notes".to_owned(), "info".to_owned(), "once".to_owned())]
    );
}
