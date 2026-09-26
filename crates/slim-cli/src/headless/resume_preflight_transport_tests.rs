use super::{run_provider_resume_with_preflight_events, ProviderRequest, ProviderRunOptions};
use slim_core::provider::{ProviderError, ProviderKind};
use slim_core::session::{preflight_session, DurableSessionHeader, JsonlRepo};
use slim_core::OperatingMode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn fixture_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-resume-preflight-transport-{}-{}.jsonl",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

#[test]
fn transported_preflight_remains_the_snapshot_used_by_resume() {
    let path = fixture_path();
    JsonlRepo::create(
        &path,
        DurableSessionHeader::new("first", "now", "D:\\Slim", None, None),
    )
    .expect("first repo");
    let preflight = preflight_session(&path).expect("preflight");
    std::fs::remove_file(&path).expect("replace fixture");
    JsonlRepo::create(
        &path,
        DurableSessionHeader::new("replacement", "later", "D:\\Slim", None, None),
    )
    .expect("replacement repo");

    let error = match run_provider_resume_with_preflight_events(
        ProviderRequest {
            prompt: "continue".into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint: "http://127.0.0.1:1".into(),
            model: "offline".into(),
            api_key: "local-fixture".into(),
            account_id: None,
            timeout: Duration::from_millis(50),
        },
        preflight,
        ProviderRunOptions::default(),
        None,
    ) {
        Ok(_) => panic!("replacement after preflight must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ProviderError::InvalidResponse { ref message }
            if message.contains("changed after read-only preflight")
    ));

    let _ = std::fs::remove_file(path);
}
