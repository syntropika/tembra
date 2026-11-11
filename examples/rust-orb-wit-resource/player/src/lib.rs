//! A compiled Orb using another Orb's sessions. `import` on the attribute names the decoder's
//! interface, read from the contract the decoder's build wrote; the installer binds it to the
//! decoder Orb. The player opens a session, decodes frame after frame through the handle and drops
//! it when done: the kernel then destroys the session in the decoder.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(
    package = "example:player@1.0.0",
    wit = "../decoder/contract",
    import = "example:decoder/decoding@1.0.0"
)]
mod player {
    use alloc::vec::Vec;

    /// The service: plays a few frames through one session of the decoder.
    #[service]
    pub struct Playback;

    impl Playback {
        /// Decode `frames` with one session keyed by `key`, and return the first byte of each
        /// decoded frame, then how many frames the session counted.
        pub fn play(key: u8, frames: Vec<Vec<u8>>) -> Vec<u32> {
            let session = decoding::open(key);
            let mut report: Vec<u32> = frames
                .into_iter()
                .map(|frame| u32::from(session.decode(&frame)[0]))
                .collect();
            report.push(session.frames());
            report
        }
    }
}
