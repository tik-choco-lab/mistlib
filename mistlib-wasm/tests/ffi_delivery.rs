#![cfg(target_arch = "wasm32")]
// Separate Node harness: the library unit-test binary is configured for browsers.
mod delivery_mapping_tests {
    use mistlib_wasm::ffi::*;
    #[wasm_bindgen_test::wasm_bindgen_test]
    fn ffi_delivery_codes_preserve_legacy_and_add_reliable_unordered() {
        use mistlib_core::types::DeliveryMethod;
        for (code, method) in [
            (DELIVERY_RELIABLE, DeliveryMethod::ReliableOrdered),
            (
                DELIVERY_UNRELIABLE_ORDERED,
                DeliveryMethod::UnreliableOrdered,
            ),
            (DELIVERY_UNRELIABLE, DeliveryMethod::Unreliable),
            (
                DELIVERY_RELIABLE_UNORDERED,
                DeliveryMethod::ReliableUnordered,
            ),
        ] {
            assert_eq!(DeliveryMethod::from_u32(code), method);
            assert_eq!(method as u32, code);
        }
        assert_eq!(DELIVERY_RELIABLE_UNORDERED, 3);
        assert_eq!(
            DeliveryMethod::from_u32(u32::MAX),
            DeliveryMethod::Unreliable
        );
    }
}
