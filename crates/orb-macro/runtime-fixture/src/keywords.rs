/// Names that are keywords of WIT, which the WIT writes with its `%` escape: the module `list`,
/// whose world is `%list` and whose contract is `contract/%list.wit`, the method `record`, and the
/// flag `r#type`, which enters the WIT as `%type`. The native fixture holds the same module.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:keywords@1.0.0", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:keywords@1.0.0")
)]
pub mod list {
    use alloc::string::String;

    /// How a word is written: a flag per field.
    #[flags]
    pub struct Marks {
        pub bold: bool,
        pub r#type: bool,
    }

    /// Records words.
    #[service]
    pub struct Words;

    impl Words {
        /// The marks of `word`: those of `marks`, and `type` when the word starts with a capital
        /// letter too.
        pub fn record(word: String, marks: Marks) -> Marks {
            Marks {
                bold: marks.bold,
                r#type: marks.r#type || word.starts_with(char::is_uppercase),
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::list::{self, Marks};
    use crate::native::{call, hosting, ready};
    use futures::FutureExt;
    use serde_json::json;

    /// The module `list` is the world `%list`, and its Orb answers the method `record` under that
    /// name, through the client and portably, the flag `r#type` crossing as `type`.
    #[test]
    fn names_that_are_wit_keywords_answer_natively() {
        let words: String = list::WIT.split_whitespace().collect();
        assert!(words.contains("world%list{exportwords;}"), "{}", list::WIT);
        let (mut pool, host) = hosting(list::orb().unwrap());
        let reference = ready(&mut pool, &host, "example:keywords/words@1.0.0");
        let client = list::WordsClient::new(reference.clone());
        let marks = Marks {
            bold: true,
            r#type: false,
        };
        let local = pool
            .run_until(host.root("alice", move |context| {
                async move { client.record(&context, "List".into(), marks).await }.boxed_local()
            }))
            .unwrap();
        assert_eq!((local.bold, local.r#type), (true, true));
        let portable = json!(["list", {"type": true}]);
        assert_eq!(
            call(&mut pool, &host, &reference, "alice", "record", portable),
            json!({"type": true})
        );
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
