use std::sync::{Arc, Mutex};

use flume::Sender;
use isahc::{AsyncReadResponseExt, Request};
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::types::openai_dialect;
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, UsageLimit,
};

use super::auth;
use crate::providers::ResolvedAuth;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "openai",
    api_key_env: "OPENAI_API_KEY",
    base_url: "https://api.openai.com/v1",
    max_tokens_field: "max_completion_tokens",
    include_stream_usage: true,
    provider_name: "OpenAI",
};
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const LABEL_SESSION: &str = "Current session";
const LABEL_WEEK: &str = "Current week";

// Non-codex models OpenAI offers for subscription usage via the Coding Plan.
// Codex models are matched by their `-codex` substring in
// `coding_plan_context_window`, so they never need listing here.
pub(crate) const PLAN_MODELS: &[&str] = &[
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2",
];

const CODEX_PLAN_CONTEXT_WINDOW: u32 = 272_000;
const GPT_5_6_PLAN_CONTEXT_WINDOW: u32 = 372_000;

fn is_codex_model(model_id: &str) -> bool {
    coding_plan_context_window(model_id).is_some()
}

// Codex models match by substring so future releases route without a registry
// edit; the named non-codex plans match exactly to avoid catching near-misses
// like `gpt-5.6-terra-preview`.
fn coding_plan_context_window(model_id: &str) -> Option<u32> {
    if model_id.contains("-codex") {
        return Some(CODEX_PLAN_CONTEXT_WINDOW);
    }
    if !PLAN_MODELS.contains(&model_id) {
        return None;
    }
    Some(if model_id.starts_with("gpt-5.6-") {
        GPT_5_6_PLAN_CONTEXT_WINDOW
    } else {
        CODEX_PLAN_CONTEXT_WINDOW
    })
}
#[derive(Deserialize)]
struct UsageResponse {
    rate_limit: Option<RateLimit>,
}

#[derive(Deserialize)]
struct RateLimit {
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    used_percent: Option<f64>,
    reset_at: Option<u64>,
    reset_after_seconds: Option<u64>,
    limit_window_seconds: Option<u64>,
}

impl UsageResponse {
    fn into_usage(self, now: u64) -> ProviderUsage {
        let Some(rate_limit) = self.rate_limit else {
            return ProviderUsage::default();
        };
        let limits = [rate_limit.primary_window, rate_limit.secondary_window]
            .into_iter()
            .flatten()
            .map(|window| {
                let label = if window
                    .limit_window_seconds
                    .is_some_and(|seconds| seconds <= 6 * 60 * 60)
                {
                    LABEL_SESSION
                } else {
                    LABEL_WEEK
                };
                UsageLimit {
                    label: label.into(),
                    percentage: window
                        .used_percent
                        .filter(|percent| percent.is_finite())
                        .map(|percent| percent.round().clamp(0.0, 100.0) as u32),
                    reset_at: window
                        .reset_at
                        .and_then(|seconds| seconds.checked_mul(1_000))
                        .or_else(|| {
                            window
                                .reset_after_seconds
                                .and_then(|seconds| seconds.checked_mul(1_000))
                                .and_then(|duration| now.checked_add(duration))
                        }),
                    detail: None,
                }
            })
            .collect();
        ProviderUsage { plan: None, limits }
    }
}

pub struct OpenAi {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    storage: Option<StateDir>,
    system_prefix: Option<String>,
    /// Env / `providers.toml` override for the platform API, resolved once at
    /// construction. Used by the Responses (codex) path only; ChatGPT Coding
    /// Plan OAuth keeps its fixed backend URL.
    resolved_base_url: Option<String>,
}

impl OpenAi {
    pub fn new(timeouts: crate::providers::Timeouts) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let resolved = auth::resolve(&storage)?;
        let compat = OpenAiCompatProvider::new(&CONFIG, timeouts);
        Ok(Self {
            resolved_base_url: resolve_openai_base_url(),
            compat,
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: crate::providers::Timeouts,
    ) -> Self {
        Self {
            resolved_base_url: resolve_openai_base_url(),
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            storage: None,
            system_prefix: None,
        }
    }
    pub(crate) fn with_oauth_storage(
        storage: StateDir,
        timeouts: crate::providers::Timeouts,
    ) -> Result<Self, AgentError> {
        let resolved = auth::resolve(&storage)?;
        Ok(Self {
            resolved_base_url: resolve_openai_base_url(),
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            system_prefix: None,
        })
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn current_auth(&self) -> ResolvedAuth {
        self.auth.lock().unwrap().clone()
    }

    fn is_oauth(&self) -> bool {
        self.storage.as_ref().is_some_and(auth::is_oauth)
    }

    async fn refresh_oauth(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let resolved = smol::unblock(move || {
            let tokens =
                maki_storage::auth::load_tokens(&storage, auth::PROVIDER).ok_or_else(|| {
                    AgentError::Api {
                        status: 401,
                        message: "OpenAI OAuth tokens not found on disk".into(),
                    }
                })?;
            match auth::refresh_tokens(&tokens) {
                Ok(fresh) => {
                    maki_storage::auth::save_tokens(&storage, auth::PROVIDER, &fresh)?;
                    Ok(auth::build_oauth_resolved(&fresh))
                }
                Err(e) => {
                    warn!(error = %e, "OpenAI OAuth refresh failed, clearing stale tokens");
                    let _ = maki_storage::auth::delete_tokens(&storage, auth::PROVIDER);
                    Err(e)
                }
            }
        })
        .await?;
        *self.auth.lock().unwrap() = resolved;
        debug!("refreshed OpenAI OAuth token");
        Ok(())
    }

    async fn with_oauth_retry<T, F, Fut>(&self, f: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let result = f().await;
        if self.is_oauth()
            && matches!(&result, Err(e) if e.is_auth_error())
            && self.refresh_oauth().await.is_ok()
        {
            return f().await;
        }
        result
    }

    fn codex_auth(&self) -> Result<ResolvedAuth, AgentError> {
        // Prefer OAuth tokens for the ChatGPT Coding Plan backend.
        if let Some(storage) = self.storage.as_ref()
            && let Some(tokens) = maki_storage::auth::load_tokens(storage, auth::PROVIDER)
        {
            return Ok(auth::build_coding_plan_resolved(&tokens));
        }
        // Fall back to standard API key via the Responses API. Env /
        // providers.toml base_url overrides the platform API only, never the
        // ChatGPT backend above.
        let mut auth = self.current_auth();
        if auth.base_url.is_none() {
            auth.base_url = self
                .resolved_base_url
                .clone()
                .or_else(|| Some(CONFIG.base_url.into()));
        }
        Ok(auth)
    }
}

fn resolve_openai_base_url() -> Option<String> {
    let config = maki_config::providers::ProvidersConfig::load();
    maki_config::providers::configured_base_url("openai", config.get("openai"))
}

impl Provider for OpenAi {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let mut buf = String::new();
            let system = super::super::with_prefix(&self.system_prefix, system, &mut buf);

            if is_codex_model(&model.id) {
                let mut body = super::responses::build_body(model, messages, system, tools);
                opts.thinking.apply_responses_reasoning(
                    &mut body,
                    openai_dialect(&model.id),
                    model,
                );
                let stream_timeout = self.compat.stream_timeout();
                return self
                    .with_oauth_retry(|| async {
                        let codex_auth = self.codex_auth()?;
                        super::responses::do_stream(
                            self.compat.client(),
                            model,
                            &body,
                            event_tx,
                            &codex_auth,
                            stream_timeout,
                        )
                        .await
                    })
                    .await;
            }

            let mut body = self.compat.build_body(model, messages, system, tools);
            opts.thinking
                .apply_reasoning_effort(&mut body, openai_dialect(&model.id), model);
            self.with_oauth_retry(|| async {
                let auth = self.current_auth();
                self.compat
                    .do_stream(model, &[], &body, event_tx, &auth)
                    .await
            })
            .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                let models = super::models()
                    .iter()
                    .flat_map(|e| e.prefixes.iter())
                    .filter(|id| is_codex_model(id))
                    .map(|&s| crate::model::ModelInfo::id_only(s.to_string()))
                    .collect();
                return Ok(models);
            }
            self.with_oauth_retry(|| async {
                let auth = self.current_auth();
                self.compat.do_list_models(&auth).await
            })
            .await
        })
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            if self.is_oauth() {
                self.refresh_oauth().await
            } else {
                Ok(())
            }
        })
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let Some(storage) = self.storage.clone() else {
                return Ok(());
            };
            let resolved = smol::unblock(move || auth::resolve(&storage)).await?;
            *self.auth.lock().unwrap() = resolved;
            debug!("reloaded OpenAI auth from storage");
            Ok(())
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async {
            if !self.is_oauth() {
                return Ok(None);
            }
            self.with_oauth_retry(|| async {
                let auth = self.codex_auth()?;
                let request = auth.configure_request(Request::get(USAGE_URL)).body(())?;
                let mut response = self.compat.client().send_async(request).await?;
                if response.status().as_u16() != 200 {
                    return Err(AgentError::from_response(response).await);
                }
                let parsed: UsageResponse = serde_json::from_str(&response.text().await?)?;
                Ok(Some(parsed.into_usage(maki_storage::auth::now_millis())))
            })
            .await
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        if self.is_oauth()
            && let Some(context_window) = coding_plan_context_window(&model.id)
        {
            model.context_window = model.context_window.min(context_window);
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const USAGE_BODY: &str = r#"{
        "plan_type": "plus",
        "rate_limit": {
            "primary_window": {"used_percent": 164.6, "reset_at": 1770415200, "limit_window_seconds": 604800},
            "secondary_window": {"used_percent": 12.2, "reset_after_seconds": 3600, "limit_window_seconds": 18000}
        }
    }"#;
    const NOW: u64 = 1_770_000_000_000;

    #[test]
    fn parse_subscription_usage_windows() {
        let parsed: UsageResponse = serde_json::from_str(USAGE_BODY).unwrap();
        let usage = parsed.into_usage(NOW);
        assert_eq!(usage.limits.len(), 2);
        assert_eq!(usage.limits[0].label, LABEL_WEEK);
        assert_eq!(usage.limits[0].percentage, Some(100));
        assert_eq!(usage.limits[0].reset_at, Some(1_770_415_200_000));
        assert_eq!(usage.limits[1].label, LABEL_SESSION);
        assert_eq!(usage.limits[1].percentage, Some(12));
        assert_eq!(usage.limits[1].reset_at, Some(NOW + 3_600_000));
    }

    #[test]
    fn parse_subscription_usage_without_rate_limit() {
        let parsed: UsageResponse = serde_json::from_str(r#"{"plan_type":"free"}"#).unwrap();
        assert!(parsed.into_usage(NOW).limits.is_empty());
    }

    #[test_case("gpt-5.6-luna")]
    #[test_case("gpt-5.6-terra")]
    #[test_case("gpt-5.6-sol")]
    fn gpt_5_6_models_use_coding_plan(model_id: &str) {
        assert!(is_codex_model(model_id));
    }

    #[test_case("gpt-5.6-luna", Some(372_000))]
    #[test_case("gpt-5.6-terra", Some(372_000))]
    #[test_case("gpt-5.6-sol", Some(372_000))]
    #[test_case("gpt-5.5", Some(272_000))]
    #[test_case("gpt-5.3-codex", Some(272_000))]
    #[test_case("gpt-5.7-codex", Some(272_000) ; "unlisted codex model still routes")]
    #[test_case("gpt-5.6-terra-preview", None ; "non-codex near-match is rejected")]
    #[test_case("gpt-5.4-nano", None)]
    fn coding_plan_context_window_resolves_plan_models(model_id: &str, expected: Option<u32>) {
        assert_eq!(coding_plan_context_window(model_id), expected);
    }
}
