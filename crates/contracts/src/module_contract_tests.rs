//! The contract of a module as `#[orb]` and `cargo tembra build` write it into an Orb crate.
use crate::{Contract, ModuleContract, WrittenContract};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use wasm_encoder::{CustomSection, Encode, Section};
use wit_parser::Resolve;

/// The contract of an Orb whose world imports the kernel's state.
const TALLY: &str = "package example:first@1.0.0;

interface tally {
  bump: func() -> u32;
}

world first {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export tally;
}
";

/// The contract another Orb's build wrote: its package, the one it imports nested whole, whose
/// world imports the kernel's state.
const SECOND: &str = "package example:second@1.0.0;

interface relay {
  twice: func() -> u32;
  thrice: func() -> u32;
}

world second {
  import example:first/tally@1.0.0;
  export relay;
}

package example:first@1.0.0 {
  interface tally {
    bump: func() -> u32;
    reset: func();
  }
  world first {
    import tembra:guest/state@1.0.0;
    export tembra:guest/state-init@1.0.0;
    export tally;
  }
}
";

/// The WIT the macro derives for a module importing the second Orb's interface.
const THIRD: &str = "package example:third@1.0.0;

interface caller {
  call: func() -> u32;
}

world third {
  import example:second/relay@1.0.0;
  export caller;
}
";

/// The WIT the macro derives for a module whose exported interface uses a package the world does
/// not import, and whose world imports another: the WIT names `greeter` first, the component's
/// types `other`, the world's import, before the package the export's `use` brings in.
const FRONT: &str = "package example:front@1.0.0;

interface front {
  use example:greeter/greeting@1.0.0.{entry};
  show: func(item: entry) -> string;
}

world w {
  import example:other/x@1.0.0;
  export front;
}
";

const GREETER: &str = "package example:greeter@1.0.0;

interface greeting {
  record entry {
    name: string,
  }
  greet: func(name: string) -> string;
}

world greeter {
  export greeting;
}
";

const OTHER: &str = "package example:other@1.0.0;

interface x {
  ping: func() -> u32;
}

world other {
  export x;
}
";

/// The contract of another Orb module of the crate, whose world imports a WASI interface: a
/// standard package other than the kernel's.
const DICE: &str = "package example:dice@1.0.0;

interface dice {
  roll: func() -> u64;
}

world roller {
  import wasi:random/random@0.3.0;
  export dice;
}
";

/// A standard package as an earlier write left it in `deps/`.
const CLOCKS: &str = "package wasi:clocks@0.3.0;\n\ninterface clock {}\n";

fn contract(package: &str, world: &str) -> String {
    format!(
        "package {package};\n\ninterface counter {{\n  add: func(by: s64) -> s64;\n}}\n\n\
         world {world} {{\n  export counter;\n}}\n"
    )
}

/// The names of a directory's entries, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The lines opening the packages nested in a contract's text.
fn nested(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with("package ") && line.ends_with('{'))
        .collect()
}

fn modified(file: &Path) -> SystemTime {
    std::fs::metadata(file).unwrap().modified().unwrap()
}

/// A component of the contract's world whose core module carries the given custom sections, as
/// the component linker keeps those of the compiled code.
fn component_carrying(contract: &Contract, sections: &[(&str, &[u8])]) -> Vec<u8> {
    let mut module = wit_component::dummy_module(
        &contract.resolve,
        contract.world,
        wit_parser::ManglingAndAbi::Standard32,
    );
    wit_component::embed_component_metadata(
        &mut module,
        &contract.resolve,
        contract.world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    for (name, data) in sections {
        let section = CustomSection {
            name: Cow::Borrowed(name),
            data: Cow::Borrowed(data),
        };
        module.push(section.id());
        section.encode(&mut module);
    }
    wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

#[test]
fn a_module_contract_is_written_with_the_standard_packages_it_depends_on() {
    let root = tempfile::tempdir().unwrap();
    let module = ModuleContract::new(TALLY, None).unwrap();
    assert_eq!((module.world(), module.text()), ("first", TALLY));
    let dir = root.path().join("contract");
    let written = module.write(root.path()).unwrap();
    assert_eq!(
        written,
        WrittenContract {
            contract: dir.join("first.wit"),
            dependencies: vec![dir.join("deps").join("tembra-guest.wit")],
            ..Default::default()
        }
    );
    assert_eq!(std::fs::read_to_string(&written.contract).unwrap(), TALLY);
    assert_eq!(names(&dir), ["deps", "first.wit"]);
    // The directory resolves on its own, as another Orb's bindings read it, and reads as the
    // contract itself.
    Resolve::default().push_dir(&dir).unwrap();
    let read = Contract::from_dir(&dir, Some("first")).unwrap();
    assert_eq!(read.prepared(), module.contract().prepared());
}

#[test]
fn a_contract_is_rewritten_only_when_its_content_changes() {
    let root = tempfile::tempdir().unwrap();
    let module = ModuleContract::new(TALLY, None).unwrap();
    let written = module.write(root.path()).unwrap();
    let files: Vec<&PathBuf> = std::iter::once(&written.contract)
        .chain(&written.dependencies)
        .collect();
    let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    for file in &files {
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(earlier)
            .unwrap();
    }
    // The same content leaves the files alone.
    assert_eq!(module.write(root.path()).unwrap(), written);
    for file in &files {
        assert_eq!(modified(file), earlier, "{}", file.display());
    }
    // Another content replaces the file, and leaves nothing else behind.
    let changed = ModuleContract::new(&TALLY.replace("bump", "bump-twice"), None).unwrap();
    assert_eq!(changed.write(root.path()).unwrap(), written);
    assert_eq!(
        std::fs::read_to_string(&written.contract).unwrap(),
        changed.text()
    );
    assert_ne!(modified(&written.contract), earlier);
    assert_eq!(modified(&written.dependencies[0]), earlier);
    assert_eq!(names(&root.path().join("contract")), ["deps", "first.wit"]);
}

#[test]
fn a_former_contract_is_a_wit_file_of_the_same_package() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("contract");
    std::fs::create_dir(&dir).unwrap();
    let text = contract("example:counter@1.0.0", "ledger");
    let files = [
        // The file being written, as an earlier write left it.
        ("ledger.wit", text.clone()),
        // The world's former name: the one to remove.
        ("tally.wit", contract("example:counter@1.0.0", "tally")),
        // Another package, another version of this one, and this one nested in another.
        ("other.wit", contract("example:other@1.0.0", "other")),
        ("next.wit", contract("example:counter@2.0.0", "next")),
        (
            "speller.wit",
            "package example:speller@1.0.0;\n\nworld speller {\n  import example:counter/counter@1.0.0;\n}\n\n\
             package example:counter@1.0.0 {\n  interface counter {\n    add: func(by: s64) -> s64;\n  }\n}\n"
                .to_owned(),
        ),
        // A file that does not parse, and one that is no .wit file.
        ("broken.wit", "package example:counter@1.0.0;\n\nworld {\n".to_owned()),
        ("notes.txt", contract("example:counter@1.0.0", "notes")),
    ];
    for (name, content) in &files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    let written = ModuleContract::new(&text, None)
        .unwrap()
        .write(root.path())
        .unwrap();
    assert_eq!(written.former_contracts, [dir.join("tally.wit")]);
    assert!(written.dependencies.is_empty() && written.former_dependencies.is_empty());
    assert_eq!(
        names(&dir),
        [
            "broken.wit",
            "ledger.wit",
            "next.wit",
            "notes.txt",
            "other.wit",
            "speller.wit"
        ]
    );
}

#[test]
fn a_standard_package_the_contract_no_longer_depends_on_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let deps = root.path().join("contract").join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    let former = deps.join("wasi-random.wit");
    let notes = deps.join("notes.wit");
    std::fs::write(
        &former,
        "package wasi:random@0.3.0;\n\ninterface random {\n  get-random-u64: func() -> u64;\n}\n",
    )
    .unwrap();
    std::fs::write(
        &notes,
        "package example:notes@1.0.0;\n\ninterface notes {}\n",
    )
    .unwrap();
    let written = ModuleContract::new(TALLY, None)
        .unwrap()
        .write(root.path())
        .unwrap();
    assert_eq!(written.former_dependencies, std::slice::from_ref(&former));
    assert_eq!(written.dependencies, [deps.join("tembra-guest.wit")]);
    assert!(!former.exists() && notes.exists());
    // The same world, once it depends on no standard package, leaves no deps/ behind.
    std::fs::remove_file(&notes).unwrap();
    let written = ModuleContract::new(&contract("example:first@1.0.0", "first"), None)
        .unwrap()
        .write(root.path())
        .unwrap();
    assert!(written.dependencies.is_empty());
    assert_eq!(written.former_dependencies, [deps.join("tembra-guest.wit")]);
    assert!(!deps.exists());
}

#[test]
fn a_consumed_package_is_nested_whole_with_the_packages_it_builds_on() {
    // The consumed directory: the other Orb's contract, as its build wrote it.
    let other = tempfile::tempdir().unwrap();
    ModuleContract::new(SECOND, None)
        .unwrap()
        .write(other.path())
        .unwrap();
    let consumed = other.path().join("contract");
    let module = ModuleContract::new(THIRD, Some(&consumed)).unwrap();
    assert_eq!(module.world(), "third");
    // The imported package whole, then the one it builds on; the kernel's package is never
    // nested, and goes to deps/ instead.
    assert!(module.text().starts_with(THIRD), "{}", module.text());
    assert_eq!(
        nested(module.text()),
        [
            "package example:second@1.0.0 {",
            "package example:first@1.0.0 {"
        ]
    );
    assert!(module.text().contains("thrice: func() -> u32;"));
    let root = tempfile::tempdir().unwrap();
    let written = module.write(root.path()).unwrap();
    assert_eq!(
        written.dependencies,
        [root.path().join("contract/deps/tembra-guest.wit")]
    );
    let read = Contract::from_dir(root.path().join("contract"), Some("third")).unwrap();
    assert_eq!(read.prepared(), module.contract().prepared());
    // A directory that does not resolve is reported with its path.
    let error = ModuleContract::new(THIRD, Some(&root.path().join("missing")))
        .err()
        .unwrap();
    assert!(
        error.0.contains("reading the consumed WIT in") && error.0.contains("missing"),
        "{error}"
    );
}

#[test]
fn a_component_carries_its_contract_and_its_types_serve_without_the_consumed_directory() {
    // The component's own types reach the imported package alone, and only what it calls.
    let types = Contract::from_source(
        "third.wit",
        &format!(
            "{THIRD}\npackage example:second@1.0.0 {{\n  interface relay {{\n    twice: func() -> u32;\n  }}\n}}\n"
        ),
        Some("third"),
    )
    .unwrap();
    // Built without the macro, a component carries no module contract.
    let plain = component_carrying(&types, &[]);
    assert!(
        ModuleContract::from_component(&plain, None)
            .unwrap()
            .is_none()
    );
    let component = component_carrying(&types, &[("tembra:wit", THIRD.as_bytes())]);
    let module = ModuleContract::from_component(&component, None)
        .unwrap()
        .unwrap();
    assert_eq!(module.world(), "third");
    assert_eq!(nested(module.text()), ["package example:second@1.0.0 {"]);
    assert!(module.text().contains("twice") && !module.text().contains("thrice"));
    // With the consumed directory, each package comes from there, whole.
    let other = tempfile::tempdir().unwrap();
    ModuleContract::new(SECOND, None)
        .unwrap()
        .write(other.path())
        .unwrap();
    let module = ModuleContract::from_component(&component, Some(&other.path().join("contract")))
        .unwrap()
        .unwrap();
    assert_eq!(
        nested(module.text()),
        [
            "package example:second@1.0.0 {",
            "package example:first@1.0.0 {"
        ]
    );
    assert!(module.text().contains("thrice"));
    assert_eq!(
        module.text(),
        ModuleContract::new(THIRD, Some(&other.path().join("contract")))
            .unwrap()
            .text()
    );
}

#[test]
fn the_macro_and_the_build_nest_the_consumed_packages_in_the_order_the_wit_names_them() {
    // The consumed directory, in the layout wit-parser reads: one package, the other in deps/.
    let consumed = tempfile::tempdir().unwrap();
    std::fs::write(consumed.path().join("greeter.wit"), GREETER).unwrap();
    std::fs::create_dir(consumed.path().join("deps")).unwrap();
    std::fs::write(consumed.path().join("deps").join("other.wit"), OTHER).unwrap();
    let checked = ModuleContract::new(FRONT, Some(consumed.path())).unwrap();
    assert_eq!(
        nested(checked.text()),
        [
            "package example:greeter@1.0.0 {",
            "package example:other@1.0.0 {"
        ]
    );
    // The component built from the same WIT meets the packages the other way round; the contract
    // the build writes is the one `cargo check` wrote, whichever source the packages come from.
    let component = component_carrying(checked.contract(), &[("tembra:wit", FRONT.as_bytes())]);
    let built = ModuleContract::from_component(&component, Some(consumed.path()))
        .unwrap()
        .unwrap();
    assert_eq!(built.text(), checked.text());
    let built = ModuleContract::from_component(&component, None)
        .unwrap()
        .unwrap();
    assert_eq!(nested(built.text()), nested(checked.text()));
}

#[test]
fn a_stale_temporary_of_an_earlier_write_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("contract");
    let deps = dir.join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    // A build killed between the write and the rename of its temporary left these behind, for a
    // former name of the world too; the young one may be a concurrent writer's, and another
    // dotfile is not this writer's, whatever its age.
    let stale = [
        dir.join(".first.wit.4242.tmp"),
        dir.join(".former.wit.4242.tmp"),
        deps.join(".tembra-guest.wit.4242.tmp"),
    ];
    let young = dir.join(".first.wit.4243.tmp");
    let other = dir.join(".notes.4242.tmp");
    let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    for file in stale.iter().chain([&young, &other]) {
        std::fs::write(file, "partial").unwrap();
    }
    for file in stale.iter().chain([&other]) {
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(earlier)
            .unwrap();
    }
    let written = ModuleContract::new(TALLY, None)
        .unwrap()
        .write(root.path())
        .unwrap();
    assert!(written.former_contracts.is_empty() && written.former_dependencies.is_empty());
    assert_eq!(
        names(&dir),
        [
            ".first.wit.4243.tmp",
            ".notes.4242.tmp",
            "deps",
            "first.wit"
        ]
    );
    assert_eq!(names(&deps), ["tembra-guest.wit"]);
}

/// The Orb modules of one crate write one `contract/`: a standard package one world depends on
/// stays while another writes, whichever writes first, and one no world depends on goes. Writing
/// them all again changes no file.
#[test]
fn the_worlds_of_one_crate_keep_the_standard_packages_each_depends_on() {
    let tally = ModuleContract::new(TALLY, None).unwrap();
    let dice = ModuleContract::new(DICE, None).unwrap();
    let plain = ModuleContract::new(&contract("example:counter@1.0.0", "ledger"), None).unwrap();
    for order in [[&tally, &dice, &plain], [&plain, &dice, &tally]] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("contract");
        let deps = dir.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        // A standard package no world of the directory depends on, which an earlier write left.
        std::fs::write(deps.join("wasi-clocks.wit"), CLOCKS).unwrap();
        for module in order {
            module.write(root.path()).unwrap();
        }
        assert_eq!(
            names(&dir),
            ["deps", "first.wit", "ledger.wit", "roller.wit"]
        );
        assert_eq!(names(&deps), ["tembra-guest.wit", "wasi-random.wit"]);
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let files: Vec<PathBuf> = ["first.wit", "ledger.wit", "roller.wit"]
            .map(|name| dir.join(name))
            .into_iter()
            .chain(["tembra-guest.wit", "wasi-random.wit"].map(|name| deps.join(name)))
            .collect();
        for file in &files {
            std::fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(earlier)
                .unwrap();
        }
        for module in order {
            let written = module.write(root.path()).unwrap();
            assert!(written.former_dependencies.is_empty(), "{written:?}");
        }
        for file in &files {
            assert_eq!(modified(file), earlier, "{}", file.display());
        }
    }
}

/// A world named like a keyword of WIT, a module `list`, is named as WIT writes it, with the `%`
/// escape: its contract is `contract/%list.wit`, as `cargo tembra build` named it before the macro
/// wrote it, and reads back as the contract of `%list`. Another world of the crate reads it so
/// too, and keeps the standard package it depends on.
#[test]
fn a_world_named_like_a_wit_keyword_is_written_with_its_escape() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("contract");
    let list = ModuleContract::new(&TALLY.replace("world first", "world %list"), None).unwrap();
    assert_eq!(list.world(), "%list");
    let written = list.write(root.path()).unwrap();
    assert_eq!(written.contract, dir.join("%list.wit"));
    let read = Contract::from_dir(&dir, Some("%list")).unwrap();
    assert_eq!(read.prepared(), list.contract().prepared());
    ModuleContract::new(&contract("example:counter@1.0.0", "ledger"), None)
        .unwrap()
        .write(root.path())
        .unwrap();
    assert_eq!(names(&dir), ["%list.wit", "deps", "ledger.wit"]);
    assert_eq!(names(&dir.join("deps")), ["tembra-guest.wit"]);
}

/// Builds of one crate may write its `contract/` at once and remove the same file: the one that
/// finds it gone has nothing left to do. Here concurrent writes of the crate's contract each find
/// a standard package no world depends on, which only one of them removes.
#[test]
fn a_file_another_build_removed_first_is_no_error() {
    let root = tempfile::tempdir().unwrap();
    crate::module_contract::remove(&root.path().join("gone.wit")).unwrap();
    let tally = ModuleContract::new(TALLY, None).unwrap();
    tally.write(root.path()).unwrap();
    let deps = root.path().join("contract").join("deps");
    for _ in 0..20 {
        std::fs::write(deps.join("wasi-clocks.wit"), CLOCKS).unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    tally.write(root.path()).unwrap();
                });
            }
        });
        assert_eq!(names(&deps), ["tembra-guest.wit"]);
    }
}

/// A selected Cargo output is exact and cleanup remains confined to the writing package.
#[test]
fn writing_into_a_selected_directory_preserves_other_packages() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("relocated/build/out");
    let first = ModuleContract::new(&contract("example:first@1.0.0", "first"), None).unwrap();
    let written = first.write_to(&output).unwrap();
    assert_eq!(written.contract, output.join("first.wit"));
    Contract::from_dir(&output, Some("first")).unwrap();
    let second = ModuleContract::new(&contract("example:second@1.0.0", "second"), None).unwrap();
    second.write_to(&output).unwrap();
    std::fs::write(output.join("notes.txt"), "retained").unwrap();
    let renamed = ModuleContract::new(&contract("example:first@1.0.0", "renamed"), None).unwrap();
    let written = renamed.write_to(&output).unwrap();
    assert_eq!(written.former_contracts, [output.join("first.wit")]);
    assert_eq!(
        std::fs::read_to_string(output.join("second.wit")).unwrap(),
        second.text()
    );
    assert_eq!(
        std::fs::read_to_string(output.join("notes.txt")).unwrap(),
        "retained"
    );
    assert!(!output.join("contract").exists());
}
