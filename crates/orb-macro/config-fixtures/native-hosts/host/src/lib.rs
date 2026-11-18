//! A native host of the Orb crates of the examples whose modules contribute to no hook. It links
//! the runtime through the facade's default features, which Cargo enables for each Orb crate's
//! `tembra` too.

use tembra as _;
use tembra_example_audit as _;
use tembra_example_book as _;
use tembra_example_counter as _;
use tembra_example_gate as _;
use tembra_example_greeter as _;
use tembra_example_import_greeter as _;
use tembra_example_ledger as _;
use tembra_example_speller as _;
use tembra_example_text as _;
use tembra_example_welcomer as _;
