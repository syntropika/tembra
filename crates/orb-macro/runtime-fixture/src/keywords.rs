/// Names that are keywords of WIT, which the WIT writes with its `%` escape: the module `list`,
/// whose world is `%list` and whose contract is `contract/%list.wit`, the method `record`, and the
/// field `type_`, which enters the WIT as `%type`.
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

    /// How a word is written.
    pub struct Marks {
        pub bold: bool,
        pub type_: bool,
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
                type_: marks.type_ || word.starts_with(char::is_uppercase),
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::list::{self, Marks};
    use crate::native::{ORB, call, hosting, ready};
    use futures::FutureExt;
    use serde_json::json;
    use tembra::contracts::{Contract, ModuleContract};

    /// The module `list` is the world `%list`, whose contract is `contract/%list.wit`, the method
    /// `record` is `%record` and the field `type_` is `%type`; the Orb declares what the plan of
    /// that world computes.
    #[test]
    fn names_that_are_wit_keywords_are_escaped_natively() {
        let words: String = list::WIT.split_whitespace().collect();
        for expected in [
            "world%list{exportwords;}",
            "recordmarks{bold:bool,%type:bool,}",
            "%record:func(word:string,marks:marks)->marks;",
        ] {
            assert!(words.contains(expected), "{expected}\n{}", list::WIT);
        }
        let module = ModuleContract::new(list::WIT, None).unwrap();
        assert_eq!(module.world(), "%list");
        let expected = Contract::from_source("%list.wit", list::WIT, Some("%list"))
            .unwrap()
            .plan(ORB, 1, &Default::default())
            .unwrap()
            .orb;
        assert_eq!(list::orb().unwrap().declaration(), &expected);
    }

    /// The Orb answers the method `record` under that name, through the client and portably, the
    /// field `type_` crossing as `type`.
    #[test]
    fn names_that_are_wit_keywords_answer_natively() {
        let (mut pool, host) = hosting(list::orb().unwrap());
        let reference = ready(&mut pool, &host, "example:keywords/words@1.0.0");
        let client = list::WordsClient::new(reference.clone());
        let marks = Marks {
            bold: true,
            type_: false,
        };
        let local = pool
            .run_until(host.root("alice", move |context| {
                async move { client.record(&context, "List".into(), marks).await }.boxed_local()
            }))
            .unwrap();
        assert_eq!((local.bold, local.type_), (true, true));
        let portable = json!(["list", {"bold": false, "type": true}]);
        assert_eq!(
            call(&mut pool, &host, &reference, "alice", "record", portable),
            json!({"bold": false, "type": true})
        );
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
