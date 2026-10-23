const PROTOCOL_SUFFIXES: [&str; 3] = ["/chat/completions", "/responses", "/messages"];

/// Replaces a trailing known protocol route of `endpoint` with `route`,
/// keeping any query string or fragment.
pub(super) fn protocol_url(endpoint: &str, route: &str) -> String {
    protocol_url_with(endpoint, route, &PROTOCOL_SUFFIXES)
}

pub(super) fn protocol_url_with(endpoint: &str, route: &str, known: &[&str]) -> String {
    fn strip_known(path: &mut String, known: &[&str]) {
        if let Some(suffix) = known.iter().find(|suffix| path.ends_with(**suffix)) {
            path.truncate(path.len() - suffix.len());
        }
    }

    if let Ok(mut url) = reqwest::Url::parse(endpoint) {
        let mut path = url.path().trim_end_matches('/').to_owned();
        strip_known(&mut path, known);
        url.set_path(&format!("{}/{route}", path.trim_end_matches('/')));
        return url.to_string();
    }

    let split = endpoint.find(['?', '#']).unwrap_or(endpoint.len());
    let (base, suffix) = endpoint.split_at(split);
    let mut base = base.trim_end_matches('/').to_owned();
    strip_known(&mut base, known);
    format!("{}/{route}{suffix}", base.trim_end_matches('/'))
}
