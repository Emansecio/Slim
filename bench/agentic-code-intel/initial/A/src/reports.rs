pub fn invoice(cents: u32) -> u32 {
    crate::billing::charge_total(cents)
}

pub fn old_invoice(cents: u32) -> u32 {
    crate::legacy::charge_total(cents)
}

// charge_total is the name printed by the historic exporter.
pub const EXPORT_TAG: &str = "billing::charge_total";
