use super::ProviderError;

pub(super) const MAX_CATALOG_BYTES: usize = 1024 * 1024;

/// Downloads a provider model catalog with a hard size bound. `label` names
/// the provider in error messages; the API key never appears in them.
pub(super) async fn fetch_bounded_catalog(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    label: &str,
) -> Result<Vec<u8>, ProviderError> {
    let redact = |message: String| super::redact_values(&message, &[api_key.to_owned()]);
    let mut response = client
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: error.is_connect() || error.is_timeout(),
            message: redact(format!(
                "{label} catalog connection: {}",
                error.without_url()
            )),
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::Remote {
            message: format!("{label} catalog failed ({})", response.status().as_u16()),
        });
    }
    let over_bound = || ProviderError::InvalidResponse {
        message: format!("{label} catalog exceeds its bound"),
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(over_bound());
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_CATALOG_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ProviderError::Transport {
            safe_to_retry: true,
            message: redact(format!("{label} catalog stream: {}", error.without_url())),
        })?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_CATALOG_BYTES)
        {
            return Err(over_bound());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
