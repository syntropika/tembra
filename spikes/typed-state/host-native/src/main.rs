//! The native host of the typed-state spike. The kernel keeps the state as JSON, as tembra's does,
//! and the host serves the guest's state imports in three ways:
//!
//! - JSON text, parsed and printed by the host (`tembra:guest/state` today, `worker.rs`);
//! - a WIT record through the dynamic `Val` API, converted to and from the kernel's JSON: what a
//!   worker that cannot know each Orb's state type at compile time would do;
//! - a WIT record through static bindings, kept as the Rust type: the cost of the canonical ABI
//!   alone, which tembra's worker cannot use because each Orb has its own state type.

use wasmtime::{Result, bail, ensure, format_err};
use serde_json::{Map, Value, json};
use std::time::Instant;
use wasmtime::component::types::Type;
use wasmtime::component::{Component, HasSelf, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({ path: "../wit", world: "orb" });
}

use bindings::spike::state::types::{Entry, Ledger};

/// `Option::context`, for wasmtime's error type.
trait Context<T> {
    fn context(self, what: &str) -> Result<T>;
}

impl<T> Context<T> for Option<T> {
    fn context(self, what: &str) -> Result<T> {
        self.ok_or_else(|| format_err!("missing {what}"))
    }
}

/// The ledger with `entries` entries, as the kernel keeps it: with Rust names and numbers
/// (`exact == false`, the JSON of today) or camelCase names and 64-bit integers as strings.
fn ledger(entries: usize, exact: bool) -> Value {
    let big = (1u64 << 60) + 7;
    let number = |value: u64| if exact { json!(value.to_string()) } else { json!(value) };
    let entries: Vec<Value> = (0..entries as u64)
        .map(|i| {
            json!({
                "id": number(big + i),
                "owner": format!("owner-{i}"),
                "amount": number(1_000_000 + i),
                "tags": ["alpha", "beta"],
            })
        })
        .collect();
    let mut object = Map::new();
    object.insert(if exact { "userId" } else { "user_id" }.into(), number(big));
    object.insert("name".into(), json!("ledger"));
    object.insert("balance".into(), if exact { json!("0") } else { json!(0) });
    object.insert("entries".into(), json!(entries));
    Value::Object(object)
}

/// The same ledger as the static bindings' Rust type.
fn ledger_static(entries: usize) -> Ledger {
    let big = (1u64 << 60) + 7;
    Ledger {
        user_id: big,
        name: "ledger".into(),
        balance: 0,
        entries: (0..entries as u64)
            .map(|i| Entry {
                id: big + i,
                owner: format!("owner-{i}"),
                amount: 1_000_000 + i,
                tags: vec!["alpha".into(), "beta".into()],
            })
            .collect(),
    }
}

/// A WIT name in camelCase, the spelling of the kernel's JSON.
fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for c in name.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// A lifted value as the kernel's JSON: 64-bit integers as strings, names in camelCase.
fn to_kernel(value: &Val) -> Result<Value> {
    Ok(match value {
        Val::U64(n) => Value::String(n.to_string()),
        Val::S64(n) => Value::String(n.to_string()),
        Val::U32(n) => json!(n),
        Val::String(s) => Value::String(s.clone()),
        Val::List(items) => Value::Array(items.iter().map(to_kernel).collect::<Result<_>>()?),
        Val::Record(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, value)| Ok((camel(name), to_kernel(value)?)))
                .collect::<Result<_>>()?,
        ),
        other => bail!("the spike does not convert {other:?}"),
    })
}

/// The kernel's JSON as a value of the given WIT type, to lower to the guest.
fn to_guest(ty: &Type, value: &Value) -> Result<Val> {
    Ok(match ty {
        Type::U64 => Val::U64(value.as_str().context("u64")?.parse()?),
        Type::S64 => Val::S64(value.as_str().context("s64")?.parse()?),
        Type::U32 => Val::U32(value.as_u64().context("u32")? as u32),
        Type::String => Val::String(value.as_str().context("string")?.to_owned()),
        Type::List(list) => {
            let item = list.ty();
            Val::List(
                value
                    .as_array()
                    .context("list")?
                    .iter()
                    .map(|v| to_guest(&item, v))
                    .collect::<Result<_>>()?,
            )
        }
        Type::Record(record) => {
            let object = value.as_object().context("record")?;
            Val::Record(
                record
                    .fields()
                    .map(|field| {
                        let v = object.get(&camel(field.name)).context("field")?;
                        Ok((field.name.to_owned(), to_guest(&field.ty, v)?))
                    })
                    .collect::<Result<_>>()?,
            )
        }
        other => bail!("the spike does not convert {other:?}"),
    })
}

/// What the dynamic host keeps: the kernel's JSON value of the state.
struct Kernel {
    value: Value,
}

/// What the static host keeps: the Rust value of the state.
struct Typed {
    value: Ledger,
}

impl bindings::spike::state::types::Host for Typed {}

impl bindings::spike::state::json_state::Host for Typed {
    fn read(&mut self) -> String {
        unreachable!()
    }
    fn update(&mut self, _: String) -> String {
        unreachable!()
    }
}

impl bindings::spike::state::typed_state::Host for Typed {
    fn read(&mut self) -> Ledger {
        self.value.clone()
    }
    fn update(&mut self, value: Ledger) -> Ledger {
        self.value = value;
        self.value.clone()
    }
}

fn dynamic_linker(engine: &Engine) -> Result<Linker<Kernel>> {
    let mut linker = Linker::<Kernel>::new(engine);
    let mut text = linker.instance("spike:state/json-state")?;
    text.func_new("read", |store, _, _, results| {
        results[0] = Val::String(serde_json::to_string(&store.data().value)?);
        Ok(())
    })?;
    text.func_new("update", |mut store, _, params, results| {
        let [Val::String(text)] = params else { bail!("update takes a string") };
        let value: Value = serde_json::from_str(text)?;
        store.data_mut().value = value;
        results[0] = Val::String(serde_json::to_string(&store.data().value)?);
        Ok(())
    })?;
    let mut typed = linker.instance("spike:state/typed-state")?;
    typed.func_new("read", |store, ty, _, results| {
        let result = ty.results().next().context("read has a result")?;
        results[0] = to_guest(&result, &store.data().value)?;
        Ok(())
    })?;
    typed.func_new("update", |mut store, ty, params, results| {
        let result = ty.results().next().context("update has a result")?;
        store.data_mut().value = to_kernel(&params[0])?;
        results[0] = to_guest(&result, &store.data().value)?;
        Ok(())
    })?;
    Ok(linker)
}

/// Call the named `bench` export `iterations` times through one call; answer the sum and the time
/// per iteration in microseconds.
fn run_dynamic(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Kernel>,
    function: &str,
    state: Value,
    iterations: u32,
) -> Result<(i64, f64)> {
    let mut store = Store::new(engine, Kernel { value: state });
    let instance: Instance = linker.instantiate(&mut store, component)?;
    let bench = instance
        .get_export_index(&mut store, None, "spike:state/bench")
        .ok_or_else(|| format_err!("no bench export"))?;
    let index = instance
        .get_export_index(&mut store, Some(&bench), function)
        .ok_or_else(|| format_err!("no {function}"))?;
    let func = instance.get_typed_func::<(u32,), (i64,)>(&mut store, &index)?;
    let start = Instant::now();
    let (sum,) = func.call(&mut store, (iterations,))?;
    Ok((sum, start.elapsed().as_secs_f64() * 1e6 / f64::from(iterations)))
}

fn run_static(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Typed>,
    state: Ledger,
    iterations: u32,
) -> Result<(i64, f64)> {
    let mut store = Store::new(engine, Typed { value: state });
    let orb = bindings::Orb::instantiate(&mut store, component, linker)?;
    let start = Instant::now();
    let sum = orb.spike_state_bench().call_run_typed(&mut store, iterations)?;
    Ok((sum, start.elapsed().as_secs_f64() * 1e6 / f64::from(iterations)))
}

/// The runs of each measurement; the table shows their median.
const RUNS: usize = 5;

fn median(mut runs: Vec<f64>) -> f64 {
    runs.sort_by(f64::total_cmp);
    runs[runs.len() / 2]
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("usage: host <component.wasm>")?;
    let engine = Engine::new(&Config::new())?;
    let component = Component::from_file(&engine, &path)?;
    let dynamic = dynamic_linker(&engine)?;
    let mut statically = Linker::<Typed>::new(&engine);
    bindings::Orb::add_to_linker::<_, HasSelf<_>>(&mut statically, |state| state)?;

    println!("| entries | encoding | µs per read+update |");
    println!("|---:|---|---:|");
    for (entries, iterations) in [(3usize, 20_000u32), (100, 2_000), (1_000, 200)] {
        let expected: i64 = (1..=i64::from(iterations)).sum();
        let mut rows = Vec::new();
        for (label, function, exact) in [
            ("JSON today", "run-json", false),
            ("JSON exact (camelCase, u64 as string)", "run-json-exact", true),
            ("WIT record, dynamic Val + kernel JSON", "run-typed", true),
        ] {
            // Warm up, then take the median of five runs, each on a fresh instance.
            run_dynamic(&engine, &component, &dynamic, function, ledger(entries, exact), 10)?;
            let mut runs = Vec::new();
            for _ in 0..RUNS {
                let (sum, micros) =
                    run_dynamic(&engine, &component, &dynamic, function, ledger(entries, exact), iterations)?;
                ensure!(sum == expected, "{label}: sum {sum} != {expected}");
                runs.push(micros);
            }
            rows.push((label, median(runs)));
        }
        run_static(&engine, &component, &statically, ledger_static(entries), 10)?;
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            let (sum, micros) =
                run_static(&engine, &component, &statically, ledger_static(entries), iterations)?;
            ensure!(sum == expected, "static: sum {sum} != {expected}");
            runs.push(micros);
        }
        rows.push(("WIT record, static bindings (reference)", median(runs)));
        for (label, micros) in rows {
            println!("| {entries} | {label} | {micros:.1} |");
        }
    }
    Ok(())
}
