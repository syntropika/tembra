#[tembra::orb(package = "example:counter@1.0.0")]
mod counter {
    #[state]
    pub struct Tally(u32);
}

fn main() {}
