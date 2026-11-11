//! The counter of `rust-orb-state`, written by hand against the kernel's private `tembra:orb`
//! package: the same lifecycle a componentized JavaScript Orb exports.
//!
//! The component exports two things. `example:counter/counter` is the public service the
//! contract names. `tembra:orb/lifecycle` is what the host calls around it: `describe` once,
//! before anything else; `create` for the Orb root and for every counter object; `validate` when
//! a reload hands the retained state to a new instance; `release` when an object goes away. The
//! component imports `tembra:orb/host` to declare, read and update each object's kernel state and
//! to log.
//!
//! The kernel keeps one JSON state per counter object. The component keeps only the handle the
//! kernel returned for it, keyed by the object's environment number.
#![no_std]
extern crate alloc;

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
};
use core::cell::RefCell;

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

// The build world: the public contract plus the private package, as the TypeScript build does
// with `Contract::source_world` before componentizing JavaScript.
wit_bindgen::generate!({
    inline: "
        package example:counter-build;

        world counter-orb {
            include example:counter/tally@1.0.0;
            include tembra:orb/source@0.1.0;
        }
    ",
    path: ["contract", "../../../crates/contracts/wit/tembra/orb.wit"],
    world: "example:counter-build/counter-orb",
    generate_all,
});

use exports::example::counter::counter::Count;
use exports::tembra::orb::lifecycle::{Implementation, Service};
use tembra::orb::host::{self, Handle, Level};

/// The state's version: the kernel refuses a reload that changes it.
const VERSION: u32 = 1;
/// The number this component gives its state validator; the kernel passes it back to `validate`.
const VALIDATE: u32 = 1;

/// The state the kernel keeps for one counter, as JSON.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Tally {
    total: i64,
    steps: u32,
}

/// The state's rule, checked before every update and on every reload.
fn check(tally: &Tally) -> Result<(), String> {
    if tally.total < 0 {
        return Err(format!("a total of {} is below zero", tally.total));
    }
    Ok(())
}

/// Environment number of a counter object -> the kernel's handle of its state.
struct States(RefCell<BTreeMap<u64, u32>>);

// SAFETY: a component instance runs on one thread, so the cell is never shared.
unsafe impl Sync for States {}

static STATES: States = States(RefCell::new(BTreeMap::new()));

/// The state of the counter object running the current call.
fn own_state() -> Result<Handle, String> {
    let environment = host::environment();
    let id = STATES
        .0
        .borrow()
        .get(&environment)
        .copied()
        .ok_or("this object declared no state")?;
    Ok(Handle {
        scope: Some(environment),
        id,
    })
}

fn read_tally(state: Handle) -> Result<Tally, String> {
    let snapshot = host::state_read(state, None)?;
    let text = snapshot.value.ok_or("the kernel sent no value")?;
    serde_json::from_str(&text).map_err(|error| format!("the state is not a tally: {error}"))
}

fn to_json(tally: &Tally) -> String {
    serde_json::to_string(tally).expect("a tally serializes")
}

fn count(tally: &Tally) -> Count {
    Count {
        total: tally.total,
        steps: tally.steps,
    }
}

struct Component;

impl exports::tembra::orb::lifecycle::Guest for Component {
    /// What this component implements; the host checks it against the contract before any
    /// `create`. Service and method names are the JavaScript aliases the host derives from WIT.
    fn describe() -> Implementation {
        Implementation {
            name: "counter".into(),
            api_version: 1,
            services: vec![Service {
                alias: "counter".into(),
                methods: vec!["add".into(), "read".into()],
                lifetime: None,
                linger: None,
                access: false,
            }],
            hooks: vec![],
            contributes: vec![],
        }
    }

    /// `service` is none for the Orb root, which has nothing to declare. A counter object
    /// declares its state here, with the validator the kernel runs on a reload.
    fn create(environment: u64, service: Option<String>) -> Result<(), String> {
        match service.as_deref() {
            None => Ok(()),
            Some("counter") => {
                let initial = to_json(&Tally::default());
                let id = host::state_declare(&initial, VERSION, Some(VALIDATE))?;
                STATES.0.borrow_mut().insert(environment, id);
                Ok(())
            }
            Some(other) => Err(format!("unknown service {other}")),
        }
    }

    fn release(environment: u64) {
        STATES.0.borrow_mut().remove(&environment);
    }

    /// The new instance's opinion of the state a reload retained.
    fn validate(callback: u32, value: String) -> Result<(), String> {
        if callback != VALIDATE {
            return Err(format!("unknown validator {callback}"));
        }
        host::log(Level::Info, &format!("validate {value}"));
        let tally: Tally = serde_json::from_str(&value)
            .map_err(|error| format!("the retained state is not a tally: {error}"))?;
        check(&tally)
    }

    async fn open(_callback: u32) -> Result<u32, String> {
        Err("this Orb declares no resources".to_string())
    }

    async fn close(_callback: u32, _value: u32) -> Result<(), String> {
        Err("this Orb declares no resources".to_string())
    }
}

impl exports::example::counter::counter::Guest for Component {
    fn add(by: i64) -> Result<Count, String> {
        let state = own_state()?;
        let mut tally = read_tally(state)?;
        tally.total = tally.total.checked_add(by).ok_or("the total overflows")?;
        tally.steps += 1;
        check(&tally)?;
        host::state_update(state, &to_json(&tally))?;
        host::log(Level::Info, &format!("add {by}"));
        Ok(count(&tally))
    }

    fn read() -> Count {
        let tally = own_state()
            .and_then(read_tally)
            .expect("the counter reads its state");
        count(&tally)
    }
}

export!(Component);
