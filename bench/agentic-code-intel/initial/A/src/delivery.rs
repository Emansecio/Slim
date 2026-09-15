use crate::billing::retry_key as active_key;

pub fn setting_name() -> &'static str {
    active_key()
}
