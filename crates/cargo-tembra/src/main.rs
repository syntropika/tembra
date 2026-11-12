//! `cargo tembra`: a Cargo subcommand for Rust Orbs, in the spirit of `cargo contract` and the
//! CosmWasm optimizer. An author writes a module with `#[tembra_orb_macro::orb]` and runs one
//! command; the WIT, the component checks, the artifact and the grant summary follow.
//!
//! ```text
//! cargo tembra new <name> --package ns:name@1.0.0 [--tembra <root>]
//! cargo tembra build [--debug] [--manifest-path <Cargo.toml>] [--out <dir>]
//! cargo tembra wit <component.wasm> [--emit <dir> --world <name>]
//! cargo tembra inspect <component.wasm> [--wit <dir> --world <name>]
//! ```
mod build;
mod component;
mod scaffold;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cargo-tembra", bin_name = "cargo tembra", version)]
#[command(about = "Create, build and package Rust Orbs for Tembra")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an Orb crate: a module with the macro, no WIT file or build script to write.
    New {
        /// Name of the crate and of the Orb.
        name: String,
        /// WIT package identity, for example example:greeter@1.0.0.
        #[arg(long)]
        package: String,
        /// Root of a tembra checkout to depend on by path. By default the checkout above the
        /// current directory or, outside one, the repository at the commit cargo-tembra was built
        /// from.
        #[arg(long)]
        tembra: Option<PathBuf>,
    },
    /// Compile the Orb, derive its WIT from the component, verify and package it into artifacts/.
    Build {
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use the debug profile instead of release.
        #[arg(long)]
        debug: bool,
        /// Where to collect the outputs, contract included, instead of artifacts/ next to
        /// Cargo.toml: one directory for every Orb of a repository.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the WIT reconstructed from a component, or write it as a file with --emit.
    Wit {
        component: PathBuf,
        #[arg(long)]
        emit: Option<PathBuf>,
        #[arg(long)]
        world: Option<String>,
    },
    /// Show imports, exports and required grants, and with --wit the verifier's verdict.
    Inspect {
        component: PathBuf,
        #[arg(long)]
        wit: Option<PathBuf>,
        #[arg(long)]
        world: Option<String>,
    },
}

fn main() -> Result<()> {
    // Invoked as `cargo tembra ...`, Cargo passes "tembra" as the first argument.
    let mut arguments: Vec<String> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("tembra") {
        arguments.remove(1);
    }
    match Cli::parse_from(arguments).command {
        Command::New {
            name,
            package,
            tembra,
        } => scaffold::new(&name, &package, tembra),
        Command::Build {
            manifest_path,
            debug,
            out,
        } => build::build(manifest_path, !debug, out),
        Command::Wit {
            component,
            emit,
            world,
        } => component::wit(&component, emit, world),
        Command::Inspect {
            component,
            wit,
            world,
        } => component::inspect(&component, wit, world),
    }
}

/// An empty directory for one test, under the system's temporary directory.
#[cfg(test)]
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cargo-tembra-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
