pub fn charge_total(cents: u32) -> u32 {
    cents + 900
}

pub fn retry_key() -> &'static str {
    "legacy.retry_limit"
}
