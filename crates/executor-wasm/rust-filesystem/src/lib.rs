//! Qualify standard WASI filesystem operations through generated component bindings.
#![no_std]
extern crate alloc;
use alloc::{format, string::String, vec, vec::Vec};
use core::cell::RefCell;
#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
use exports::example::compiled_filesystem::probe::{ErrorReport, Report};
use wasi::filesystem::{
    preopens,
    types::{
        Advice, Descriptor, DescriptorFlags as Flags, DescriptorType, ErrorCode, NewTimestamp,
        OpenFlags, PathFlags,
    },
};
use wit_bindgen::{FutureReader, StreamReader, StreamResult};
struct Local<T>(RefCell<T>);
// SAFETY: this fixture's wasm32 component has one thread.
unsafe impl<T> Sync for Local<T> {}
type Read = (StreamReader<u8>, FutureReader<Result<(), ErrorCode>>);
static RETAINED: Local<Option<Read>> = Local(RefCell::new(None));
fn show(error: ErrorCode) -> String {
    format!("{error:?}")
}
fn check(value: bool, label: &str) -> Result<(), String> {
    if value {
        Ok(())
    } else {
        Err(String::from(label))
    }
}
fn preopen(mount: &str) -> Result<Descriptor, String> {
    preopens::get_directories()
        .into_iter()
        .find(|(_, name)| name == mount)
        .map(|(root, _)| root)
        .ok_or_else(|| format!("missing preopen {mount}"))
}
async fn open(
    root: &Descriptor,
    name: &str,
    flags: OpenFlags,
    access: Flags,
) -> Result<Descriptor, String> {
    root.open_at(PathFlags::empty(), String::from(name), flags, access)
        .await
        .map_err(show)
}
async fn write(file: &Descriptor, bytes: Vec<u8>, offset: Option<u64>) -> Result<(), String> {
    let (mut writer, reader) = wit_stream::new::<u8>();
    let done = if let Some(offset) = offset {
        file.write_via_stream(reader, offset)
    } else {
        file.append_via_stream(reader)
    };
    let remaining = writer.write_all(bytes).await;
    drop(writer);
    done.await.map_err(show)?;
    check(remaining.is_empty(), "write stream ended early")
}
async fn read(mut pair: Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        let (status, chunk) = pair.0.read(Vec::with_capacity(32768)).await;
        bytes.extend(chunk);
        if !matches!(status, StreamResult::Complete(_)) {
            break;
        }
    }
    drop(pair.0);
    pair.1.await.map_err(show)?;
    Ok(bytes)
}
async fn refuse<T>(
    result: Result<T, ErrorCode>,
    label: &str,
    log: &mut Vec<String>,
) -> Result<(), String> {
    match result {
        Err(error) => {
            log.push(format!("{label}:{error:?}"));
            Ok(())
        }
        Ok(_) => Err(format!("{label}: unexpectedly succeeded")),
    }
}
struct Probe;
impl exports::example::compiled_filesystem::probe::Guest for Probe {
    async fn roundtrip(mount: String) -> Result<Report, String> {
        let root = preopen(&mount)?;
        check(
            matches!(
                root.get_type().await.map_err(show)?,
                DescriptorType::Directory
            ),
            "root type",
        )?;
        check(
            root.get_flags().await.map_err(show)?.bits() & Flags::MUTATE_DIRECTORY.bits() != 0,
            "root flags",
        )?;
        root.create_directory_at(String::from("roundtrip"))
            .await
            .map_err(show)?;
        let directory = open(
            &root,
            "roundtrip",
            OpenFlags::DIRECTORY,
            Flags::MUTATE_DIRECTORY,
        )
        .await?;
        directory
            .create_directory_at(String::from("nested"))
            .await
            .map_err(show)?;
        let nested = open(
            &directory,
            "nested",
            OpenFlags::DIRECTORY,
            Flags::MUTATE_DIRECTORY,
        )
        .await?;
        let file = open(
            &directory,
            "data.bin",
            OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
            Flags::READ | Flags::WRITE,
        )
        .await?;
        let bytes: Vec<u8> = (0..150000).map(|i| (i % 251) as u8).collect();
        write(&file, bytes.clone(), Some(3)).await?;
        write(&file, vec![255, 128, 0], None).await?;
        let received = read(file.read_via_stream(0)).await?;
        let large_offset = match read(file.read_via_stream(u64::MAX - 7)).await {
            Ok(bytes) => {
                check(bytes.is_empty(), "read beyond EOF returned data")?;
                String::from("eof")
            }
            Err(error) => error,
        };
        check(
            received.len() == 150006
                && received[..3] == [0, 0, 0]
                && received[3..150003] == bytes
                && received[150003..] == [255, 128, 0],
            "offset/binary/append roundtrip",
        )?;
        check(
            read(file.read_via_stream(149999)).await? == received[149999..],
            "nonzero read offset",
        )?;
        file.set_size(9).await.map_err(show)?;
        file.set_size(12).await.map_err(show)?;
        let extended = read(file.read_via_stream(9)).await?;
        check(extended == [0, 0, 0], "set-size zero extension")?;
        file.set_size(9).await.map_err(show)?;
        check(
            file.stat().await.map_err(show)?.size == 9,
            "truncate metadata",
        )?;
        check(
            file.get_flags().await.map_err(show)?.bits() & (Flags::READ | Flags::WRITE).bits()
                == (Flags::READ | Flags::WRITE).bits(),
            "file flags",
        )?;
        check(
            matches!(
                file.get_type().await.map_err(show)?,
                DescriptorType::RegularFile
            ),
            "file type",
        )?;
        for advice in [
            Advice::Normal,
            Advice::Sequential,
            Advice::Random,
            Advice::WillNeed,
            Advice::DontNeed,
            Advice::NoReuse,
        ] {
            file.advise(0, 9, advice).await.map_err(show)?;
        }
        file.sync_data().await.map_err(show)?;
        file.sync().await.map_err(show)?;
        directory.sync().await.map_err(show)?;
        file.set_times(NewTimestamp::NoChange, NewTimestamp::NoChange)
            .await
            .map_err(show)?;
        directory
            .set_times_at(
                PathFlags::empty(),
                String::from("data.bin"),
                NewTimestamp::NoChange,
                NewTimestamp::NoChange,
            )
            .await
            .map_err(show)?;
        let hash = file.metadata_hash().await.map_err(show)?;
        let at = directory
            .metadata_hash_at(PathFlags::empty(), String::from("data.bin"))
            .await
            .map_err(show)?;
        check(
            hash.lower == at.lower && hash.upper == at.upper,
            "descriptor/path metadata hash",
        )?;
        let truncated = open(&directory, "truncate.bin", OpenFlags::CREATE, Flags::WRITE).await?;
        write(&truncated, vec![9, 8, 7], Some(0)).await?;
        drop(truncated);
        let truncated = open(
            &directory,
            "truncate.bin",
            OpenFlags::TRUNCATE,
            Flags::READ | Flags::WRITE,
        )
        .await?;
        check(
            truncated.stat().await.map_err(show)?.size == 0,
            "open truncate",
        )?;
        drop(truncated);
        directory
            .unlink_file_at(String::from("truncate.bin"))
            .await
            .map_err(show)?;
        directory
            .rename_at(
                String::from("data.bin"),
                &nested,
                String::from("renamed.bin"),
            )
            .await
            .map_err(show)?;
        let reopened = open(&nested, "renamed.bin", OpenFlags::empty(), Flags::READ).await?;
        check(
            file.is_same_object(&reopened).await,
            "rename preserves descriptor identity",
        )?;
        check(
            !file.is_same_object(&directory).await,
            "different descriptor identities",
        )?;
        check(
            nested
                .stat_at(PathFlags::empty(), String::from("renamed.bin"))
                .await
                .map_err(show)?
                .size
                == 9,
            "stat-at renamed",
        )?;
        let (mut entries, done) = nested.read_directory();
        let mut listing = Vec::new();
        loop {
            let (status, chunk) = entries.read(Vec::with_capacity(1)).await;
            for entry in chunk {
                check(
                    matches!(entry.type_, DescriptorType::RegularFile),
                    "directory entry type",
                )?;
                listing.push(entry.name);
            }
            if !matches!(status, StreamResult::Complete(_)) {
                break;
            }
        }
        drop(entries);
        done.await.map_err(show)?;
        drop(reopened);
        drop(file);
        nested
            .unlink_file_at(String::from("renamed.bin"))
            .await
            .map_err(show)?;
        drop(nested);
        let directory_rename = match directory
            .rename_at(String::from("nested"), &directory, String::from("moved"))
            .await
        {
            Ok(()) => "moved",
            Err(ErrorCode::Unsupported) => "nested",
            Err(error) => return Err(show(error)),
        };
        directory
            .remove_directory_at(String::from(directory_rename))
            .await
            .map_err(show)?;
        drop(directory);
        root.remove_directory_at(String::from("roundtrip"))
            .await
            .map_err(show)?;
        Ok(Report {
            length: received.len() as u64,
            large_offset,
            listing,
            directory_rename: String::from(directory_rename),
            hash_lower: hash.lower,
            hash_upper: hash.upper,
        })
    }
    async fn errors(mount: String, readonly_mount: String) -> Result<ErrorReport, String> {
        let root = preopen(&mount)?;
        let readonly = preopen(&readonly_mount)?;
        let file = open(&root, "input.txt", OpenFlags::empty(), Flags::READ).await?;
        let view = open(&readonly, "input.txt", OpenFlags::empty(), Flags::READ).await?;
        check(
            read(view.read_via_stream(0)).await? == b"seeded",
            "readonly mount reads",
        )?;
        check(
            file.is_same_object(&view).await,
            "shared storage identity across read-only mount",
        )?;
        let mut log = Vec::new();
        refuse(
            root.open_at(
                PathFlags::empty(),
                String::from("missing"),
                OpenFlags::empty(),
                Flags::READ,
            )
            .await,
            "missing",
            &mut log,
        )
        .await?;
        refuse(
            root.open_at(
                PathFlags::empty(),
                String::from("input.txt"),
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                Flags::WRITE,
            )
            .await,
            "exclusive",
            &mut log,
        )
        .await?;
        root.create_directory_at(String::from("child"))
            .await
            .map_err(show)?;
        for path in ["../escape", "/escape", "child/../../escape"] {
            refuse(
                root.stat_at(PathFlags::empty(), String::from(path)).await,
                &format!("traversal:{path}"),
                &mut log,
            )
            .await?;
        }
        root.remove_directory_at(String::from("child"))
            .await
            .map_err(show)?;
        refuse(
            root.stat_at(PathFlags::empty(), String::from("bad\0name"))
                .await,
            "nul",
            &mut log,
        )
        .await?;
        refuse(
            file.create_directory_at(String::from("wrong")).await,
            "wrong-kind",
            &mut log,
        )
        .await?;
        refuse(file.set_size(1).await, "readonly-file-size", &mut log).await?;
        refuse(
            view.set_times(NewTimestamp::Now, NewTimestamp::NoChange)
                .await,
            "readonly-file-times",
            &mut log,
        )
        .await?;
        let (stream, done) = root.read_via_stream(0);
        drop(stream);
        refuse(done.await, "read-directory-as-file", &mut log).await?;
        let (entries, done) = file.read_directory();
        drop(entries);
        refuse(done.await, "read-file-as-directory", &mut log).await?;
        refuse(
            readonly
                .create_directory_at(String::from("forbidden"))
                .await,
            "readonly-directory",
            &mut log,
        )
        .await?;
        refuse(
            readonly
                .open_at(
                    PathFlags::empty(),
                    String::from("forbidden"),
                    OpenFlags::CREATE,
                    Flags::WRITE,
                )
                .await,
            "readonly-open",
            &mut log,
        )
        .await?;
        let hard_link = match root
            .link_at(
                PathFlags::empty(),
                String::from("input.txt"),
                &root,
                String::from("linked"),
            )
            .await
        {
            Ok(()) => {
                let linked = open(&root, "linked", OpenFlags::empty(), Flags::READ).await?;
                check(
                    file.is_same_object(&linked).await,
                    "hard link preserves identity",
                )?;
                drop(linked);
                root.unlink_file_at(String::from("linked"))
                    .await
                    .map_err(show)?;
                String::from("ok")
            }
            Err(error) => show(error),
        };
        let symlink = match root
            .symlink_at(String::from("input.txt"), String::from("symlink"))
            .await
        {
            Ok(()) => {
                check(
                    root.readlink_at(String::from("symlink"))
                        .await
                        .map_err(show)?
                        == "input.txt",
                    "symbolic link target",
                )?;
                root.unlink_file_at(String::from("symlink"))
                    .await
                    .map_err(show)?;
                String::from("ok")
            }
            Err(error) => show(error),
        };
        refuse(
            root.readlink_at(String::from("input.txt")).await,
            "readlink-file",
            &mut log,
        )
        .await?;
        refuse(
            root.readlink_at(String::from("missing")).await,
            "readlink-missing",
            &mut log,
        )
        .await?;
        let writable = open(&root, "input.txt", OpenFlags::empty(), Flags::WRITE).await?;
        let timestamp = match writable
            .set_times(NewTimestamp::NoChange, NewTimestamp::Now)
            .await
        {
            Ok(()) => String::from("ok"),
            Err(error) => show(error),
        };
        let timestamp_at = match root
            .set_times_at(
                PathFlags::empty(),
                String::from("input.txt"),
                NewTimestamp::Now,
                NewTimestamp::NoChange,
            )
            .await
        {
            Ok(()) => String::from("ok"),
            Err(error) => show(error),
        };
        let timestamp_missing = match root
            .set_times_at(
                PathFlags::empty(),
                String::from("missing"),
                NewTimestamp::NoChange,
                NewTimestamp::NoChange,
            )
            .await
        {
            Ok(()) => String::from("ok"),
            Err(error) => show(error),
        };
        Ok(ErrorReport {
            checked: log,
            hard_link,
            symlink,
            timestamp,
            timestamp_at,
            timestamp_missing,
        })
    }
    async fn retain(mount: String) -> Result<(), String> {
        let root = preopen(&mount)?;
        let file = open(&root, "input.txt", OpenFlags::empty(), Flags::READ).await?;
        *RETAINED.0.borrow_mut() = Some(file.read_via_stream(0));
        drop(file);
        Ok(())
    }
    async fn read_retained() -> Result<Vec<u8>, String> {
        let pair = RETAINED
            .0
            .borrow_mut()
            .take()
            .ok_or_else(|| String::from("nothing retained"))?;
        read(pair).await
    }
    fn drop_retained() {
        RETAINED.0.borrow_mut().take();
    }
    async fn pending_read(mount: String) -> Result<Vec<u8>, String> {
        let root = preopen(&mount)?;
        let file = open(&root, "input.txt", OpenFlags::empty(), Flags::READ).await?;
        read(file.read_via_stream(0)).await
    }
}
export!(Probe);
