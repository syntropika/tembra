use super::*;

#[test]
fn contract_scope_is_required_even_when_other_options_are_valid() {
    let error = contract(
        quote!(name = "scope.required", version = 1),
        quote!(
            trait MissingScope {}
        ),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "contract requires exactly one explicit scope: local or remote"
    );
}

#[test]
fn every_contract_option_refuses_duplicates_or_conflicting_scope() {
    for options in [
        quote!(local, local, name = "a", version = 1),
        quote!(remote, remote, name = "a", version = 1, host = "host"),
        quote!(local, remote, name = "a", version = 1, host = "host"),
        quote!(remote, local, name = "a", version = 1),
        quote!(local, name = "a", name = "b", version = 1),
        quote!(local, name = "a", version = 1, version = 2),
        quote!(local, name = "a", version = 1, input = (), input = ()),
        quote!(remote, name = "a", version = 1, host = "a", host = "b"),
        quote!(
            remote,
            name = "a",
            version = 1,
            host = "a",
            variant = "a",
            variant = "b"
        ),
    ] {
        let error = contract(
            options,
            quote!(
                trait Checked {}
            ),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate or conflicting contract option"),
            "{error}"
        );
    }
}

#[test]
fn helper_attributes_are_checked_before_expansion() {
    for (member, expected) in [
        (
            quote!(
                #[state]
                #[state]
                fn count(&self) -> u32;
            ),
            "duplicate contract member attribute",
        ),
        (
            quote!(
                #[portable]
                #[portable]
                async fn read(&self) -> Result<u32>;
            ),
            "duplicate contract member attribute",
        ),
        (
            quote!(
                #[opaque(id = "a@1")]
                #[opaque(id = "b@1")]
                fn value(&self) -> u32;
            ),
            "duplicate contract member attribute",
        ),
        (
            quote!(
                #[state(foo)]
                fn count(&self) -> u32;
            ),
            "state does not accept arguments",
        ),
        (
            quote!(
                #[portable(foo)]
                async fn read(&self) -> Result<u32>;
            ),
            "portable does not accept arguments",
        ),
        (
            quote!(
                #[opaque]
                fn value(&self) -> u32;
            ),
            "opaque requires id",
        ),
        (
            quote!(
                #[opaque()]
                fn value(&self) -> u32;
            ),
            "opaque requires a nonempty id",
        ),
        (
            quote!(
                #[opaque(id = "")]
                fn value(&self) -> u32;
            ),
            "opaque requires a nonempty id",
        ),
        (
            quote!(
                #[opaque(id = "bad")]
                fn value(&self) -> u32;
            ),
            "opaque id must be name@version",
        ),
        (
            quote!(
                #[opaque(id = "a@1", id = "b@1")]
                fn value(&self) -> u32;
            ),
            "duplicate opaque id",
        ),
        (
            quote!(
                #[state]
                #[portable]
                fn count(&self) -> u32;
            ),
            "state/opaque getters cannot use portable",
        ),
        (
            quote!(
                #[opaque(id = "a@1")]
                #[portable]
                fn value(&self) -> u32;
            ),
            "state/opaque getters cannot use portable",
        ),
    ] {
        let error = contract(
            quote!(local, name = "checked", version = 1),
            quote!(trait Checked {#member}),
        )
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}
