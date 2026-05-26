use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use claurst_core::config::Config;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

const FREELLMAPI_PROVIDER_ID: &str = "freellmapi";
const FREELLMAPI_DEFAULT_BASE_URL: &str = "http://127.0.0.1:3001";
const FREELLMAPI_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const FREELLMAPI_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
struct ApiKeyResponse {
    #[serde(rename = "apiKey")]
    api_key: String,
}

pub async fn bootstrap_if_needed(
    cwd: &Path,
    config: &mut Config,
    provider_explicit: bool,
    model_explicit: bool,
    api_key_explicit: bool,
) -> Result<()> {
    let explicit_freellmapi_provider =
        matches!(config.provider.as_deref(), Some(FREELLMAPI_PROVIDER_ID));
    let explicit_freellmapi_model =
        matches!(config.model.as_deref(), Some(model) if model.starts_with("freellmapi/"));
    let explicit_freellmapi = explicit_freellmapi_provider || explicit_freellmapi_model;
    let model_is_auto = matches!(config.model.as_deref(), Some("auto"));
    let should_default = !provider_explicit && (!model_explicit || model_is_auto);

    if !explicit_freellmapi && !should_default {
        return Ok(());
    }

    match ensure_ready(cwd, config).await {
        Ok((origin, api_key)) => {
            let entry = config
                .provider_configs
                .entry(FREELLMAPI_PROVIDER_ID.to_string())
                .or_default();
            entry.api_base = Some(origin);
            entry.api_key = Some(api_key);

            // A top-level API key belongs to whichever provider was active
            // before bootstrap. Once we switch to FreeLLMAPI, let the freshly
            // discovered local key win unless the user explicitly overrode it.
            if !api_key_explicit {
                config.api_key = None;
            }

            if should_default || explicit_freellmapi_model {
                config.provider = Some(FREELLMAPI_PROVIDER_ID.to_string());
            }
        }
        Err(err) if should_default => {
            warn!(error = %err, "FreeLLMAPI bootstrap failed; falling back to existing provider defaults");
        }
        Err(err) => return Err(err),
    }

    Ok(())
}

async fn ensure_ready(cwd: &Path, config: &Config) -> Result<(String, String)> {
    let origin = freellmapi_origin(config);

    if !is_healthy(&origin).await {
        let server_dir = find_server_dir(cwd).with_context(|| {
            format!(
                "FreeLLMAPI is not running at {} and no local checkout was found. Set FREELLMAPI_ROOT or start the server manually.",
                origin
            )
        })?;

        start_server(&server_dir)
            .await
            .with_context(|| format!("Failed to start FreeLLMAPI from {}", server_dir.display()))?;

        wait_until_healthy(&origin).await.with_context(|| {
            format!(
                "FreeLLMAPI did not become ready after starting from {}",
                server_dir.display()
            )
        })?;
    }

    let api_key = fetch_api_key(&origin).await.with_context(|| {
        format!(
            "Failed to fetch FreeLLMAPI API key from {}/api/settings/api-key",
            origin
        )
    })?;

    Ok((origin, api_key))
}

fn freellmapi_origin(config: &Config) -> String {
    let configured = config
        .resolve_provider_api_base(FREELLMAPI_PROVIDER_ID)
        .unwrap_or_else(|| FREELLMAPI_DEFAULT_BASE_URL.to_string());
    strip_v1_suffix(&configured)
}

fn strip_v1_suffix(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some(stripped) = trimmed.strip_suffix("/v1") {
        stripped.trim_end_matches('/').to_string()
    } else {
        trimmed.to_string()
    }
}

async fn is_healthy(origin: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };

    match client.get(format!("{}/api/health", origin)).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

async fn fetch_api_key(origin: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client for FreeLLMAPI")?;

    let payload = client
        .get(format!("{}/api/settings/api-key", origin))
        .send()
        .await
        .context("FreeLLMAPI API key request failed")?
        .error_for_status()
        .context("FreeLLMAPI API key endpoint returned an error")?
        .json::<ApiKeyResponse>()
        .await
        .context("Failed to parse FreeLLMAPI API key response")?;

    anyhow::ensure!(
        !payload.api_key.trim().is_empty(),
        "FreeLLMAPI returned an empty API key"
    );

    Ok(payload.api_key)
}

async fn start_server(server_dir: &Path) -> Result<()> {
    let mut cmd = if server_dir.join("dist/index.js").is_file() {
        let mut cmd = Command::new("node");
        cmd.arg("dist/index.js");
        cmd
    } else if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "npm run dev"]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-lc", "npm run dev"]);
        cmd
    };

    cmd.current_dir(server_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);

    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x0800_0000u32);
    }

    cmd.spawn()?;
    info!(server_dir = %server_dir.display(), "Started FreeLLMAPI in the background");
    Ok(())
}

async fn wait_until_healthy(origin: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + FREELLMAPI_STARTUP_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if is_healthy(origin).await {
            return Ok(());
        }
        sleep(FREELLMAPI_POLL_INTERVAL).await;
    }

    anyhow::bail!("FreeLLMAPI never reported healthy at {}", origin)
}

fn find_server_dir(cwd: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(root) = std::env::var("FREELLMAPI_ROOT") {
        let root = PathBuf::from(root);
        let server_dir = if root.ends_with("server") {
            root
        } else {
            root.join("server")
        };
        candidates.push(server_dir);
    }

    for ancestor in cwd.ancestors() {
        candidates.push(ancestor.join("freellmapi").join("server"));
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            candidates.push(ancestor.join("freellmapi").join("server"));
        }
    }

    candidates.into_iter().find(|dir| {
        dir.join("package.json").is_file()
            && (dir.join("dist/index.js").is_file() || dir.join("src/index.ts").is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::strip_v1_suffix;

    #[test]
    fn strips_v1_suffix_when_present() {
        assert_eq!(
            strip_v1_suffix("http://127.0.0.1:3001/v1"),
            "http://127.0.0.1:3001"
        );
        assert_eq!(
            strip_v1_suffix("http://127.0.0.1:3001/v1/"),
            "http://127.0.0.1:3001"
        );
    }

    #[test]
    fn preserves_origin_without_v1_suffix() {
        assert_eq!(
            strip_v1_suffix("http://127.0.0.1:3001"),
            "http://127.0.0.1:3001"
        );
    }
}
