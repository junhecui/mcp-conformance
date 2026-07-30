//! P3-04: classify each observed connection destination (`observe::connection_log`, P3-02)
//! as `InSandbox` or `External` — architecture.md §4.4's "instrumented arm" step, and the
//! input `openWorldHint` (P3-05) evaluates next.
//!
//! # Why this lives in `normalise`, not `verdict` or `observe`
//!
//! `observe`'s own contract is "must not interpret anything" — deciding what an address
//! *means* is already interpretation, so it cannot live there. `verdict`'s contract is
//! `(canonical_evidence, protocol_version) -> verdict`: it evaluates already-classified
//! evidence against protocol rules, the same way it consumes [`datamodel::CanonicalChangeset`]
//! rather than raw paths — it does not itself turn raw evidence into canonical form. That
//! turning is exactly what this crate already does for filesystem paths (`classify`, ADR-008);
//! classifying destinations is the same kind of pure `(raw_evidence, context) -> taxonomy`
//! step, over a different evidence surface.
//!
//! # Why the bridge subnet is a parameter, not a constant here
//!
//! `10.200.0.0/30` is `sandbox::netns`'s own choice of bridge addresses, not a fact about
//! networking in general — hardcoding it in this `no_std`, dependency-free crate would
//! silently couple two crates that otherwise know nothing about each other. A caller
//! (`orchestrator`, in practice, the one crate permitted to depend on both `sandbox` and
//! `normalise`) supplies it.

use datamodel::{ClassifiedDestination, DestinationClass, Ipv4Network, ObservedDestination};

/// Loopback is a fixed, universal IPv4 fact (`127.0.0.0/8`), not a project-specific choice —
/// unlike the bridge subnet, this is safe to hardcode here.
const LOOPBACK_NETWORK: Ipv4Network = Ipv4Network::new([127, 0, 0, 0], 8);

/// Classify one observed destination: [`DestinationClass::InSandbox`] if it falls inside
/// loopback or `bridge_network` (`sandbox::netns::NetworkBridge`'s own subnet, supplied by
/// the caller — see this module's own doc comment for why it isn't hardcoded here),
/// [`DestinationClass::External`] otherwise.
#[must_use]
pub fn classify_destination(
    destination: &ObservedDestination,
    bridge_network: Ipv4Network,
) -> DestinationClass {
    if in_network(destination.address, LOOPBACK_NETWORK) || in_network(destination.address, bridge_network) {
        DestinationClass::InSandbox
    } else {
        DestinationClass::External
    }
}

/// Classify every destination in `destinations`, in the same order they arrived in — the
/// network-evidence analogue of [`crate::normalise`]'s path classification, but destinations
/// have no natural sort key worth imposing (unlike paths, which `normalise` sorts by raw
/// bytes for order-independence), so this preserves `observe::connection_log`'s own
/// acceptance order instead.
#[must_use]
pub fn classify_destinations(
    destinations: &[ObservedDestination],
    bridge_network: Ipv4Network,
) -> alloc::vec::Vec<ClassifiedDestination> {
    destinations
        .iter()
        .map(|destination| ClassifiedDestination {
            destination: *destination,
            class: classify_destination(destination, bridge_network),
        })
        .collect()
}

/// Whether `address` falls inside `network`, by the standard prefix-mask test. `prefix_len =
/// 0` (matching every address) is handled explicitly: a `u32` left-shifted by 32 is undefined
/// behaviour's shell-language cousin in C, but well-defined-yet-wrong in Rust (a panic in
/// debug builds, a wrapping shift in release) — spelled out as its own case rather than
/// relying on the arithmetic to happen to do the right thing.
fn in_network(address: [u8; 4], network: Ipv4Network) -> bool {
    if network.prefix_len == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - u32::from(network.prefix_len));
    (u32::from_be_bytes(address) & mask) == (u32::from_be_bytes(network.address) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const BRIDGE: Ipv4Network = Ipv4Network::new([10, 200, 0, 0], 30);

    const fn destination(address: [u8; 4], port: u16) -> ObservedDestination {
        ObservedDestination::new(address, port)
    }

    #[test]
    fn loopback_is_in_sandbox() {
        assert_eq!(
            classify_destination(&destination([127, 0, 0, 1], 80), BRIDGE),
            DestinationClass::InSandbox
        );
        // The whole /8, not just 127.0.0.1 specifically.
        assert_eq!(
            classify_destination(&destination([127, 42, 0, 7], 80), BRIDGE),
            DestinationClass::InSandbox
        );
    }

    #[test]
    fn an_address_inside_the_bridge_subnet_is_in_sandbox() {
        assert_eq!(
            classify_destination(&destination([10, 200, 0, 1], 443), BRIDGE),
            DestinationClass::InSandbox
        );
        assert_eq!(
            classify_destination(&destination([10, 200, 0, 2], 443), BRIDGE),
            DestinationClass::InSandbox
        );
    }

    #[test]
    fn an_address_just_outside_the_bridge_subnet_is_external() {
        // The bridge is a /30 covering only .0-.3; .4 is the very next network.
        assert_eq!(
            classify_destination(&destination([10, 200, 0, 4], 443), BRIDGE),
            DestinationClass::External
        );
    }

    #[test]
    fn an_arbitrary_public_address_is_external() {
        assert_eq!(
            classify_destination(&destination([93, 184, 216, 34], 80), BRIDGE),
            DestinationClass::External
        );
    }

    #[test]
    fn a_private_address_outside_the_bridge_subnet_is_still_external() {
        // Being RFC1918-private does not make an address part of *this* sandbox's own
        // bridge — the sandboxed netns has exactly one route out, and it goes through the
        // bridge subnet specifically, nothing broader.
        assert_eq!(
            classify_destination(&destination([192, 168, 1, 1], 80), BRIDGE),
            DestinationClass::External
        );
    }

    #[test]
    fn classify_destinations_preserves_input_order() {
        let destinations = vec![
            destination([93, 184, 216, 34], 80),
            destination([127, 0, 0, 1], 22),
            destination([10, 200, 0, 1], 9999),
        ];
        let classified = classify_destinations(&destinations, BRIDGE);
        assert_eq!(
            classified.iter().map(|c| c.class).collect::<vec::Vec<_>>(),
            vec![DestinationClass::External, DestinationClass::InSandbox, DestinationClass::InSandbox]
        );
        assert_eq!(
            classified.iter().map(|c| c.destination).collect::<vec::Vec<_>>(),
            destinations
        );
    }
}
