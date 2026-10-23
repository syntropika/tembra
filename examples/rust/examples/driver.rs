//! The Driver is Tembra's scheduler contract: `spawn`, `sleep` and `now`. The manual driver
//! keeps a virtual clock, so sleeping costs no real time.

use std::time::Duration;
use tembra::{Driver, ManualDriver};

fn main() {
    let driver = ManualDriver::new();

    // 1. `spawn` queues a job. Nothing runs until the driver is driven.
    let clock = driver.clone();
    driver.spawn(Box::pin(async move {
        println!("  [A] start           t={:?}", clock.now());
        clock.sleep(Duration::from_secs(2)).await; // "wake me up in two seconds"
        println!("  [A] done after 2s   t={:?}", clock.now());
    }));

    let clock = driver.clone();
    driver.spawn(Box::pin(async move {
        println!("  [B] start and done  t={:?}", clock.now());
    }));

    // 2. `run_until` drives every queued job until this future completes. When nothing can
    //    progress, the manual clock jumps to the next deadline instead of waiting for it.
    let clock = driver.clone();
    driver.run_until(async move {
        println!("[main] start          t={:?}", clock.now());
        clock.sleep(Duration::from_secs(3)).await;
        println!("[main] done after 3s  t={:?}", clock.now());
    });
    println!("Finished. Real seconds elapsed: zero.");
}
