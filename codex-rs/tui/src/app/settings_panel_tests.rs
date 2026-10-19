use super::*;
use pretty_assertions::assert_eq;

#[test]
fn session_flags_win_over_what_config_read_reports() {
    let mut effective_config: ConfigReadResponse = serde_json::from_value(serde_json::json!({
        "config": { "features": { "apps": false, "plugins": false } },
        "origins": {},
    }))
    .expect("valid config read response");
    let session_flags = serde_json::json!({ "connectors": true })
        .as_object()
        .cloned();

    overlay_session_flag_features(&mut effective_config, session_flags.as_ref());

    // The legacy `connectors` flag still turns apps on for every new thread.
    assert_eq!(
        (
            feature_enabled_from_effective_config(&effective_config, Feature::Apps),
            feature_enabled_from_effective_config(&effective_config, Feature::Plugins),
        ),
        (true, false)
    );
}
