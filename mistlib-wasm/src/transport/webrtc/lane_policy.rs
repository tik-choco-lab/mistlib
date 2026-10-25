/// Only seq-0 bulk may use a writer independent of the sequenced FIFO.
pub fn use_bulk_writer(control_priority: bool, data: &[u8]) -> bool {
    control_priority && mistlib_core::transport::send::SendClass::from_wire(data).unsequenced_bulk
}

pub fn flush_reliable_queue(method: mistlib_core::types::DeliveryMethod, bulk: bool) -> bool {
    method == mistlib_core::types::DeliveryMethod::ReliableOrdered && !bulk
}

/// Resolve a missing/not-open optional channel to the stronger reliable method.
pub fn physical_method(
    method: mistlib_core::types::DeliveryMethod,
    unordered_open: bool,
) -> mistlib_core::types::DeliveryMethod {
    use mistlib_core::types::DeliveryMethod;
    if method == DeliveryMethod::ReliableUnordered && !unordered_open {
        DeliveryMethod::ReliableOrdered
    } else {
        method
    }
}
