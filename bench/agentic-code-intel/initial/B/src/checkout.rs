use crate::billing::charge_total as calculate;

pub fn submit(cents: u32) -> u32 {
    calculate(cents)
}

pub const LABEL: &str = "charge_total";
