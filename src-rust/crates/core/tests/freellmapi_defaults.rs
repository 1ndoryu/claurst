use claurst_core::config::{
    api_base_env_var_for_provider, api_key_env_vars_for_provider, default_api_base_for_provider,
    Config,
};

#[test]
fn freellmapi_provider_maps_env_vars_and_default_base() {
    assert_eq!(
        api_key_env_vars_for_provider("freellmapi"),
        ["FREELLMAPI_API_KEY"]
    );
    assert_eq!(
        api_base_env_var_for_provider("freellmapi"),
        Some("FREELLMAPI_BASE_URL")
    );
    assert_eq!(
        default_api_base_for_provider("freellmapi"),
        Some("http://127.0.0.1:3001")
    );
}

#[test]
fn freellmapi_defaults_to_auto_model() {
    let mut config = Config::default();
    config.provider = Some("freellmapi".to_string());

    assert_eq!(config.effective_model(), "auto");
}
