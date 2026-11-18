//! The kernel's rules on what an Orb declares, as public functions: the kernel checks them when it
//! installs an Orb and acquires an object, and the `#[tembra::orb]` macro checks the same ones at
//! `cargo check`, through tembra-contracts.
use tembra_core::{
    HOST_PRINCIPAL, Lifetime, RESERVED_MEMBERS, admits_linger, admits_principal, reserved_member,
    valid_version,
};

#[test]
fn reserved_members_are_the_members_every_reference_has() {
    for member in RESERVED_MEMBERS {
        assert!(reserved_member(member), "{member}");
    }
    for member in ["read", "Then", "released", ""] {
        assert!(!reserved_member(member), "{member}");
    }
}

#[test]
fn versions_start_at_one() {
    assert!(!valid_version(0));
    assert!(valid_version(1));
    assert!(valid_version(u32::MAX));
}

#[test]
fn a_host_lifetime_object_never_lingers_and_belongs_to_the_host() {
    assert!(admits_linger(Lifetime::Host, 0));
    assert!(!admits_linger(Lifetime::Host, 5));
    assert!(admits_linger(Lifetime::Referenced, 5));
    assert!(admits_principal(Lifetime::Host, HOST_PRINCIPAL));
    assert!(!admits_principal(Lifetime::Host, "alice"));
    assert!(admits_principal(Lifetime::Referenced, "alice"));
}
