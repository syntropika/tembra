//! A compiled Orb using another Orb's builders. `import` on the attribute names the text Orb's
//! interface, read from the contract the text Orb's build wrote; the installer binds it to the
//! `text` Orb. The speller starts a builder, pushes the letters of a word one by one through the
//! handle, reads the text and drops the handle: the kernel then destroys the builder.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(
    package = "example:speller@1.0.0",
    wit = "../text/contract",
    import = "example:text/texts@1.0.0"
)]
mod speller {
    use alloc::string::String;

    /// The service: spells words, one builder of the text Orb per word.
    #[service]
    pub struct Spelling;

    impl Spelling {
        /// Spell `word` letter by letter, a dash between the letters, through one builder: as many
        /// `push` calls as letters and dashes, all on the same handle.
        pub fn spell(word: String) -> String {
            let builder = texts::start();
            for (index, letter) in word.chars().enumerate() {
                if index > 0 {
                    builder.push('-');
                }
                builder.push(letter);
            }
            builder.text()
        }
    }
}
