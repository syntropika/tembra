//! A compiled test Orb that exercises fuel, traps and the standard WASI filesystem grant.
#![no_std]
extern crate alloc;
use alloc::{borrow::ToOwned, format, string::String};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use wasi::filesystem::{
    preopens,
    types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags},
};

struct Probe;

impl exports::example::probe::probe::Guest for Probe {
    /// Spend fuel in guest code; the checksum prevents the loop from being removed.
    fn spin(rounds: u32) -> u32 {
        let mut checksum = 0u32;
        for round in 0..rounds {
            checksum = core::hint::black_box(checksum.wrapping_mul(31).wrapping_add(round));
        }
        checksum
    }

    /// Stop the component with a Wasm trap.
    fn trap() {
        core::arch::wasm32::unreachable()
    }

    /// Open a path for reading and close it again.
    async fn open(path: String) -> Result<(), String> {
        let (directory, relative) = resolve(&path)?;
        directory
            .open_at(
                PathFlags::empty(),
                relative.as_str().into(),
                OpenFlags::empty(),
                DescriptorFlags::READ,
            )
            .await
            .map(drop)
            .map_err(|error| format!("{error:?}"))
    }

    /// Create a new empty file, which a read-only grant must refuse.
    async fn create_file(path: String) -> Result<(), String> {
        let (directory, relative) = resolve(&path)?;
        directory
            .open_at(
                PathFlags::empty(),
                relative.as_str().into(),
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                DescriptorFlags::WRITE,
            )
            .await
            .map(drop)
            .map_err(|error| format!("{error:?}"))
    }

    /// Create a directory, an operation outside the source Orb text API.
    async fn create_directory(path: String) -> Result<(), String> {
        let (directory, relative) = resolve(&path)?;
        directory
            .create_directory_at(relative.as_str().into())
            .await
            .map_err(|error| format!("{error:?}"))
    }

    /// Remove an empty directory.
    async fn remove_directory(path: String) -> Result<(), String> {
        let (directory, relative) = resolve(&path)?;
        directory
            .remove_directory_at(relative.as_str().into())
            .await
            .map_err(|error| format!("{error:?}"))
    }
}

/// Select the granted preopen whose mount contains an absolute guest path.
fn resolve(path: &str) -> Result<(Descriptor, String), String> {
    for (directory, mount) in preopens::get_directories() {
        if let Some(relative) = path
            .strip_prefix(mount.as_str())
            .and_then(|suffix| suffix.strip_prefix('/'))
        {
            return Ok((directory, relative.to_owned()));
        }
    }
    Err(format!("{path} is outside every preopen"))
}

export!(Probe);
