//! `library::orb(..)` in a native host: the Orb's `#[root]` creates the catalogue once, when the
//! host installs the Orb, and every shelf reads that one value, in its create and in its methods.
use futures::{FutureExt, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, Reference, contracts::Contract};
use tembra_orb_native_fixture::library;

const SHELF: &str = "calculator::example:library/shelf@1.0.0";
const PORTER: &str = "calculator::example:library/porter@1.0.0";

/// What happened in the Orb, in order, as its root and its shelves report it to the host.
type Events = Rc<RefCell<Vec<String>>>;

/// Shelve a book on each shelf in turn, under a root context: how many books the library holds
/// after each.
fn shelve(pool: &mut LocalPool, host: &Host, shelves: &[Rc<Reference>]) -> Vec<u32> {
    let shelves = shelves.to_vec();
    pool.run_until(host.root("host", move |context| {
        async move {
            let mut books = Vec::new();
            for shelf in &shelves {
                books.push(shelf.call::<_, u32>(&context, "shelve", ()).await?);
            }
            Ok(books)
        }
        .boxed_local()
    }))
    .unwrap()
}

/// The root runs once, when the host installs the Orb, and not when `orb(..)` builds it, which
/// declares the plan of the module's contract, but for the porter. Each shelf's create reads the
/// catalogue the root created, and so does each shelf's method: the books shelved on two shelves
/// are what a third shelf's create finds and its method counts on.
#[test]
fn the_root_runs_once_and_every_object_reads_its_value() {
    let events = Events::default();
    let orb = library::orb(library::Inputs {
        events: events.clone(),
    })
    .unwrap();
    let expected = Contract::from_source("library.wit", library::WIT, Some("library"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    // A contract plans no service for an empty interface: the porter, which has no method, is
    // declared by the Orb alone.
    let mut declared = orb.declaration().clone();
    declared
        .services
        .retain(|service| !service.methods.is_empty());
    assert_eq!(declared, expected);
    assert!(events.borrow().is_empty(), "{:?}", events.borrow());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    assert_eq!(*events.borrow(), ["root"]);
    let get =
        |key: &str, principal: &str| Rc::new(host.get(SHELF, vec![key.into()], principal).unwrap());
    let first = [get("a", "alice"), get("b", "alice")];
    assert_eq!(shelve(&mut pool, &host, &first), [1, 2]);
    let third = get("a", "bob");
    assert_eq!(shelve(&mut pool, &host, std::slice::from_ref(&third)), [3]);
    assert_eq!(
        *events.borrow(),
        [
            "root",
            "shelf a/alice with 0",
            "shelf b/alice with 0",
            "shelf a/bob with 2"
        ]
    );
    for shelf in first.iter().chain([&third]) {
        shelf.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
}

/// A porter has no method, and the Orb declares it without a signature, which only a contract's
/// service has. An object of it runs the task its create declared while it is referenced, and the
/// task stops when its last reference is released.
#[test]
fn a_service_without_methods_runs_its_task_while_referenced() {
    let events = Events::default();
    let orb = library::orb(library::Inputs {
        events: events.clone(),
    })
    .unwrap();
    let declared = orb
        .declaration()
        .services
        .iter()
        .find(|service| service.name == PORTER)
        .unwrap();
    assert!(declared.methods.is_empty(), "{declared:?}");
    assert_eq!(declared.signature, None);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let porter = host.get(PORTER, vec!["main".into()], "alice").unwrap();
    pool.run_until_stalled();
    assert_eq!(*events.borrow(), ["root", "porter main/alice starts"]);
    porter.release().unwrap();
    pool.run_until_stalled();
    assert_eq!(
        *events.borrow(),
        [
            "root",
            "porter main/alice starts",
            "porter main/alice stops"
        ]
    );
    pool.run_until(host.close()).unwrap();
}

/// A method that takes the call's `&Context` reads the root's value through it, with
/// `Context::orb::<T>()`, as a create reads it through its declaration: the books two shelves
/// shelved are the catalogue's, which a method of either counts.
#[test]
fn a_method_reads_the_root_through_its_context() {
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![library::orb(library::Inputs { events }).unwrap()])
        .unwrap();
    let get = |key: &str| Rc::new(host.get(SHELF, vec![key.into()], "alice").unwrap());
    let shelves = [get("a"), get("b")];
    assert_eq!(shelve(&mut pool, &host, &shelves), [1, 2]);
    let counted = shelves[0].clone();
    let books = pool
        .run_until(host.root("host", move |context| {
            async move { counted.call::<_, u32>(&context, "books", ()).await }.boxed_local()
        }))
        .unwrap();
    assert_eq!(books, 2);
    for shelf in &shelves {
        shelf.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
}
