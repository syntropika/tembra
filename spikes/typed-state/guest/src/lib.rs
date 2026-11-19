//! The guest of the typed-state spike: the same read, change, update loop over three encodings.

wit_bindgen::generate!({ path: "../wit", world: "orb" });

use exports::spike::state::bench::Guest;
use spike::state::{json_state, typed_state};

/// The state as an author writes it today: Rust names, serde's default numbers.
mod plain {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    pub struct Entry {
        pub id: u64,
        pub owner: String,
        pub amount: u64,
        pub tags: Vec<String>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Ledger {
        pub user_id: u64,
        pub name: String,
        pub balance: i64,
        pub entries: Vec<Entry>,
    }
}

/// The same state with the attributes a macro could add: camelCase, 64-bit integers as strings.
mod exact {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Entry {
        #[serde(with = "text")]
        pub id: u64,
        pub owner: String,
        #[serde(with = "text")]
        pub amount: u64,
        pub tags: Vec<String>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Ledger {
        #[serde(with = "text")]
        pub user_id: u64,
        pub name: String,
        #[serde(with = "text")]
        pub balance: i64,
        pub entries: Vec<Entry>,
    }

    /// A 64-bit integer as its decimal string.
    mod text {
        use serde::{Deserialize, Deserializer, Serializer, de::Error};
        use std::{fmt::Display, str::FromStr};

        pub fn serialize<T: Display, S: Serializer>(value: &T, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(value)
        }

        pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
        where
            T: FromStr<Err: Display>,
            D: Deserializer<'de>,
        {
            <&str>::deserialize(deserializer)?.parse().map_err(D::Error::custom)
        }
    }
}

struct Component;

impl Guest for Component {
    fn run_json(iterations: u32) -> i64 {
        let mut sum = 0;
        for _ in 0..iterations {
            let mut ledger: plain::Ledger = serde_json::from_str(&json_state::read()).unwrap();
            ledger.balance += 1;
            let kept = json_state::update(&serde_json::to_string(&ledger).unwrap());
            sum += serde_json::from_str::<plain::Ledger>(&kept).unwrap().balance;
        }
        sum
    }

    fn run_json_exact(iterations: u32) -> i64 {
        let mut sum = 0;
        for _ in 0..iterations {
            let mut ledger: exact::Ledger = serde_json::from_str(&json_state::read()).unwrap();
            ledger.balance += 1;
            let kept = json_state::update(&serde_json::to_string(&ledger).unwrap());
            sum += serde_json::from_str::<exact::Ledger>(&kept).unwrap().balance;
        }
        sum
    }

    fn run_typed(iterations: u32) -> i64 {
        let mut sum = 0;
        for _ in 0..iterations {
            let mut ledger = typed_state::read();
            ledger.balance += 1;
            sum += typed_state::update(&ledger).balance;
        }
        sum
    }
}

export!(Component);
