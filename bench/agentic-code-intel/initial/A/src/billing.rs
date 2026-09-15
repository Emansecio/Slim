pub fn charge_total(cents: u32) -> u32 {
    cents + 25
}

pub fn retry_key() -> &'static str {
    "transport.retry_limit"
}
