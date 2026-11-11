//! Standard clocks, randomness and binary CLI exercised through a real compiled component.
#![no_std]
extern crate alloc;
use alloc::{format, string::String, vec::Vec};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
use exports::example::system::probe::{
    ClockReport, EnvironmentReport, RandomReport, TransferReport,
};
use wasi::{cli, clocks, random};
use wit_bindgen::StreamResult;

async fn write(stderr: bool, bytes: Vec<u8>) -> Result<(), String> {
    let (mut writer, reader) = wit_stream::new::<u8>();
    let completion = if stderr {
        cli::stderr::write_via_stream(reader)
    } else {
        cli::stdout::write_via_stream(reader)
    };
    let remaining = writer.write_all(bytes).await;
    drop(writer);
    let result = completion.await.map_err(|error| format!("{error:?}"));
    result?;
    if !remaining.is_empty() {
        return Err(String::from("output closed before all bytes were accepted"));
    }
    Ok(())
}

struct Probe;
impl exports::example::system::probe::Guest for Probe {
    fn clocks() -> ClockReport {
        let instant = clocks::system_clock::now();
        ClockReport {
            seconds: instant.seconds,
            nanoseconds: instant.nanoseconds,
            system_resolution: clocks::system_clock::get_resolution(),
            monotonic: clocks::monotonic_clock::now(),
            monotonic_resolution: clocks::monotonic_clock::get_resolution(),
        }
    }

    fn random(length: u64) -> RandomReport {
        RandomReport {
            secure: random::random::get_random_bytes(length),
            insecure: random::insecure::get_insecure_random_bytes(length),
            secure_word: random::random::get_random_u64(),
            insecure_word: random::insecure::get_insecure_random_u64(),
            seed: random::insecure_seed::get_insecure_seed(),
            next_seed: random::insecure_seed::get_insecure_seed(),
        }
    }

    fn environment() -> EnvironmentReport {
        EnvironmentReport {
            environment: cli::environment::get_environment(),
            arguments: cli::environment::get_arguments(),
            cwd: cli::environment::get_initial_cwd(),
            terminals: (
                cli::terminal_stdin::get_terminal_stdin().is_none(),
                cli::terminal_stdout::get_terminal_stdout().is_none(),
                cli::terminal_stderr::get_terminal_stderr().is_none(),
            ),
        }
    }

    async fn wait_until(when: u64) {
        clocks::monotonic_clock::wait_until(when).await;
    }

    async fn wait_for(duration: u64) {
        // Let qualification acknowledge real guest entry before cancelling the following wait.
        let _ = clocks::monotonic_clock::now();
        clocks::monotonic_clock::wait_for(duration).await;
    }

    async fn transfer() -> Result<TransferReport, String> {
        let (mut input, completion) = cli::stdin::read_via_stream();
        let mut report = TransferReport {
            bytes: 0,
            checksum: 0,
        };
        loop {
            let (status, bytes) = input.read(Vec::with_capacity(65536)).await;
            report.bytes += bytes.len() as u64;
            for byte in &bytes {
                report.checksum = report.checksum.wrapping_mul(31).wrapping_add(*byte as u64);
            }
            write(false, bytes).await?;
            if !matches!(status, StreamResult::Complete(_)) {
                break;
            }
        }
        drop(input);
        completion.await.map_err(|error| format!("{error:?}"))?;
        write(true, alloc::vec![0, 255, 128, 10]).await?;
        Ok(report)
    }

    async fn read_drop() -> Result<u32, String> {
        let (mut input, completion) = cli::stdin::read_via_stream();
        let (_, bytes) = input.read(Vec::with_capacity(1)).await;
        drop(input);
        completion.await.map_err(|error| format!("{error:?}"))?;
        Ok(bytes.len() as u32)
    }

    async fn write(stderr: bool, bytes: Vec<u8>) -> Result<(), String> {
        write(stderr, bytes).await
    }

    async fn write_pair(bytes: Vec<u8>) -> Result<(), String> {
        let (mut first, first_input) = wit_stream::new::<u8>();
        let (mut second, second_input) = wit_stream::new::<u8>();
        let one = cli::stdout::write_via_stream(first_input);
        let two = cli::stderr::write_via_stream(second_input);
        first.write_all(bytes.clone()).await;
        drop(first);
        second.write_all(bytes).await;
        drop(second);
        one.await.map_err(|error| format!("{error:?}"))?;
        two.await.map_err(|error| format!("{error:?}"))?;
        Ok(())
    }

    fn stop(code: u8) {
        cli::exit::exit_with_code(code);
    }
    fn exit(success: bool) {
        cli::exit::exit(if success { Ok(()) } else { Err(()) });
    }
    fn ping() -> String {
        String::from("alive")
    }
}
export!(Probe);
