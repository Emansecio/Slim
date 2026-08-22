pub fn authorize_http(header: &str, expected_token: &str) -> bool {
    header.trim() == format!("Bearer {expected_token}")
}
