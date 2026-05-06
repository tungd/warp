use std::{collections::HashMap, fs, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use uuid::Uuid;

const LOCAL_ACCOUNT_FILE: &str = "local-account.json";
const LOCAL_LLM_FILE: &str = "llm.toml";
const TOKEN_TTL_SECONDS: &str = "3600";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LocalAccount {
    user_id: String,
    device_id: String,
    display_name: String,
    id_token: String,
    refresh_token: String,
    custom_token: String,
}

impl LocalAccount {
    fn load_or_create() -> Result<Self> {
        let path = account_path();
        if path.exists() {
            let mut account: Self = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))
                .and_then(|contents| {
                    serde_json::from_str(&contents)
                        .with_context(|| format!("failed to parse {}", path.display()))
                })?;
            if account.sync_local_identity() {
                let contents = serde_json::to_string_pretty(&account)?;
                fs::write(&path, contents)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            return Ok(account);
        }

        let account = Self {
            user_id: local_user_id(),
            device_id: local_device_id(),
            display_name: local_display_name(),
            id_token: format!("local-id-token-{}", Uuid::new_v4()),
            refresh_token: format!("local-refresh-token-{}", Uuid::new_v4()),
            custom_token: format!("local-custom-token-{}", Uuid::new_v4()),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let contents = serde_json::to_string_pretty(&account)?;
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(account)
    }

    fn sync_local_identity(&mut self) -> bool {
        let user_id = local_user_id();
        let device_id = local_device_id();
        let display_name = local_display_name();
        let changed = self.user_id != user_id
            || self.device_id != device_id
            || self.display_name != display_name;
        if changed {
            self.user_id = user_id;
            self.device_id = device_id;
            self.display_name = display_name;
        }
        changed
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmConfig {
    base_url: Option<String>,
    token: Option<String>,
    api_style: Option<String>,
    model: Option<String>,
    model_name: Option<String>,
    name: Option<String>,
    id: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    models: Vec<LocalLlmConfigModel>,
}

impl LocalLlmConfig {
    fn load() -> Self {
        let path = llm_config_path();
        if !path.exists() {
            return Self::default();
        }

        match fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .and_then(|contents| {
                toml::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))
            }) {
            Ok(config) => config,
            Err(err) => {
                log::warn!("Ignoring local LLM config: {err:#}");
                Self::default()
            }
        }
    }

    fn models(&self) -> Vec<ResolvedLocalLlm> {
        let models = self
            .models
            .iter()
            .filter_map(|model| {
                ResolvedLocalLlm::from_parts(
                    model.id.as_deref(),
                    model.display_name.as_deref(),
                    model
                        .model
                        .as_deref()
                        .or(model.model_name.as_deref())
                        .or(model.name.as_deref()),
                    model.base_url.as_deref().or(self.base_url.as_deref()),
                    model.api_style.as_deref().or(self.api_style.as_deref()),
                    model.token.as_deref().or(self.token.as_deref()),
                )
            })
            .collect::<Vec<_>>();

        if !models.is_empty() {
            return models;
        }

        ResolvedLocalLlm::from_parts(
            self.id.as_deref(),
            self.display_name.as_deref(),
            self.model
                .as_deref()
                .or(self.model_name.as_deref())
                .or(self.name.as_deref()),
            self.base_url.as_deref(),
            self.api_style.as_deref(),
            self.token.as_deref(),
        )
        .into_iter()
        .collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmConfigModel {
    base_url: Option<String>,
    token: Option<String>,
    api_style: Option<String>,
    model: Option<String>,
    model_name: Option<String>,
    name: Option<String>,
    id: Option<String>,
    display_name: Option<String>,
}

#[derive(Clone, Debug)]
struct ResolvedLocalLlm {
    id: String,
    display_name: String,
    base_model_name: String,
    base_url: String,
    api_style: String,
    token_configured: bool,
}

impl ResolvedLocalLlm {
    fn from_parts(
        id: Option<&str>,
        display_name: Option<&str>,
        model: Option<&str>,
        base_url: Option<&str>,
        api_style: Option<&str>,
        token: Option<&str>,
    ) -> Option<Self> {
        let model = model?.trim();
        if model.is_empty() {
            return None;
        }

        let api_style = api_style.unwrap_or("openai").trim();
        let base_url = base_url.unwrap_or("").trim();
        let id = id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "local-{}-{}",
                    sanitize_identifier(api_style),
                    sanitize_identifier(model)
                )
            });

        Some(Self {
            id,
            display_name: display_name
                .map(str::trim)
                .filter(|display_name| !display_name.is_empty())
                .unwrap_or(model)
                .to_owned(),
            base_model_name: model.to_owned(),
            base_url: base_url.to_owned(),
            api_style: api_style.to_owned(),
            token_configured: token.is_some_and(|token| !token.trim().is_empty()),
        })
    }

    fn provider(&self) -> &'static str {
        match self.api_style.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => "ANTHROPIC",
            "google" | "gemini" => "GOOGLE",
            "openai" | "openai-compatible" | "openai_compatible" => "OPENAI",
            "xai" | "grok" => "XAI",
            _ => "UNKNOWN",
        }
    }

    fn description(&self) -> String {
        let auth = if self.token_configured {
            "token configured"
        } else {
            "no token configured"
        };
        if self.base_url.is_empty() {
            format!("Local {} model ({auth})", self.api_style)
        } else {
            format!(
                "Local {} model at {} ({auth})",
                self.api_style, self.base_url
            )
        }
    }
}

#[derive(Clone)]
struct ServerState {
    account: Arc<LocalAccount>,
}

pub struct LoopbackServer {
    _runtime: Runtime,
    server_root_url: String,
}

impl LoopbackServer {
    pub fn spawn() -> Result<Self> {
        let account = Arc::new(LocalAccount::load_or_create()?);
        let state = ServerState { account };

        let std_listener = std::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .context("failed to bind OSS loopback server")?;
        std_listener
            .set_nonblocking(true)
            .context("failed to configure OSS loopback listener")?;
        let addr = std_listener
            .local_addr()
            .context("failed to read OSS loopback listener address")?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("warp-oss-loopback")
            .worker_threads(1)
            .enable_all()
            .build()
            .context("failed to create OSS loopback runtime")?;

        let router = Router::new()
            .route("/healthz", get(healthz))
            .route("/graphql/v2", post(graphql_v2))
            .route("/proxy/customToken", post(proxy_token))
            .route("/proxy/token", post(proxy_token))
            .with_state(state);

        runtime.spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(listener) => listener,
                Err(err) => {
                    log::warn!("Failed to adopt OSS loopback listener: {err:#}");
                    return;
                }
            };
            if let Err(err) = axum::serve(listener, router).await {
                log::warn!("OSS loopback server exited: {err:#}");
            }
        });

        let server_root_url = format!("http://{addr}");
        log::info!("Started OSS loopback server at {server_root_url}");

        Ok(Self {
            _runtime: runtime,
            server_root_url,
        })
    }

    pub fn server_root_url(&self) -> &str {
        &self.server_root_url
    }
}

async fn healthz(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "userId": state.account.user_id,
        "deviceId": state.account.device_id,
    }))
}

async fn proxy_token(State(state): State<ServerState>) -> Json<Value> {
    Json(firebase_token_response(&state.account))
}

async fn graphql_v2(
    State(state): State<ServerState>,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    let operation_name = params
        .get("op")
        .and_then(|op| (!op.is_empty()).then_some(op.as_str()))
        .or_else(|| body.get("operationName").and_then(Value::as_str))
        .unwrap_or_default();

    let response = match operation_name {
        "CreateAnonymousUser" | "createAnonymousUser" => {
            create_anonymous_user_response(&state.account)
        }
        "GetUser" | "getUser" => get_user_response(&state.account),
        "GetUserSettings" | "getUserSettings" => get_user_settings_response(),
        "GetFeatureModelChoices" | "getFeatureModelChoices" => get_feature_model_choices_response(),
        "SetUserIsOnboarded" | "setUserIsOnboarded" => set_user_is_onboarded_response(),
        unknown => {
            log::warn!("OSS loopback received unsupported GraphQL operation: {unknown}");
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "errors": [{
                        "message": format!("unsupported local GraphQL operation: {unknown}")
                    }]
                })),
            )
                .into_response();
        }
    };

    Json(response).into_response()
}

fn create_anonymous_user_response(account: &LocalAccount) -> Value {
    json!({
        "data": {
            "createAnonymousUser": {
                "__typename": "CreateAnonymousUserOutput",
                "expiresAt": null,
                "anonymousUserType": "NATIVE_CLIENT_ANONYMOUS_USER_FEATURE_GATED",
                "firebaseUid": account.user_id,
                "idToken": account.custom_token,
                "isInviteValid": true,
                "responseContext": response_context(),
            }
        }
    })
}

fn get_user_response(account: &LocalAccount) -> Value {
    json!({
        "data": {
            "user": {
                "__typename": "UserOutput",
                "apiKeyOwnerType": null,
                "principalType": "USER",
                "user": {
                    "anonymousUserInfo": null,
                    "experiments": [],
                    "isOnboarded": true,
                    "isOnWorkDomain": false,
                    "profile": {
                        "displayName": account.display_name,
                        "email": "",
                        "needsSsoLink": false,
                        "photoUrl": null,
                        "uid": account.user_id,
                    },
                    "llms": feature_model_choice(),
                },
            }
        }
    })
}

fn get_user_settings_response() -> Value {
    json!({
        "data": {
            "user": {
                "__typename": "UserOutput",
                "user": {
                    "settings": {
                        "isCloudConversationStorageEnabled": false,
                        "isCrashReportingEnabled": false,
                        "isTelemetryEnabled": false,
                    }
                }
            }
        }
    })
}

fn get_feature_model_choices_response() -> Value {
    json!({
        "data": {
            "user": {
                "__typename": "UserOutput",
                "user": {
                    "workspaces": [{
                        "featureModelChoice": feature_model_choice(),
                    }]
                }
            }
        }
    })
}

fn set_user_is_onboarded_response() -> Value {
    json!({
        "data": {
            "setUserIsOnboarded": {
                "__typename": "SetUserIsOnboardedOutput",
                "responseContext": response_context(),
            }
        }
    })
}

fn firebase_token_response(account: &LocalAccount) -> Value {
    json!({
        "expiresIn": TOKEN_TTL_SECONDS,
        "idToken": account.id_token,
        "refreshToken": account.refresh_token,
    })
}

fn response_context() -> Value {
    json!({
        "serverVersion": "oss-loopback",
    })
}

fn feature_model_choice() -> Value {
    let config = LocalLlmConfig::load();
    let models = config.models();
    let local = if models.is_empty() {
        available_llms(&[ResolvedLocalLlm {
            id: "local".to_string(),
            display_name: "Local Model".to_string(),
            base_model_name: "local".to_string(),
            base_url: String::new(),
            api_style: "local".to_string(),
            token_configured: false,
        }])
    } else {
        available_llms(&models)
    };
    json!({
        "agentMode": local.clone(),
        "planning": local.clone(),
        "coding": local.clone(),
        "cliAgent": local.clone(),
        "computerUseAgent": local,
    })
}

fn available_llms(models: &[ResolvedLocalLlm]) -> Value {
    let default_id = models
        .first()
        .map(|model| model.id.as_str())
        .unwrap_or("local");
    json!({
        "defaultId": default_id,
        "preferredCodexModelId": null,
        "choices": models.iter().map(llm_info).collect::<Vec<_>>(),
    })
}

fn llm_info(model: &ResolvedLocalLlm) -> Value {
    json!({
        "displayName": model.display_name,
        "baseModelName": model.base_model_name,
        "id": model.id,
        "reasoningLevel": null,
        "usageMetadata": {
            "creditMultiplier": null,
            "requestMultiplier": 0,
        },
        "description": model.description(),
        "disableReason": null,
        "visionSupported": false,
        "spec": null,
        "provider": model.provider(),
        "hostConfigs": [{
            "enabled": true,
            "modelRoutingHost": "DIRECT_API",
        }],
        "pricing": {
            "discountPercentage": null,
        },
        "contextWindow": {
            "isConfigurable": true,
            "min": 1024,
            "max": 200000,
            "default": 32000,
        },
    })
}

fn account_path() -> PathBuf {
    warp_core::paths::config_local_dir().join(LOCAL_ACCOUNT_FILE)
}

fn llm_config_path() -> PathBuf {
    warp_core::paths::config_local_dir().join(LOCAL_LLM_FILE)
}

fn local_user_id() -> String {
    format!("local-user-{}", sanitize_identifier(&local_username()))
}

fn local_device_id() -> String {
    format!("local-device-{}", sanitize_identifier(&local_hostname()))
}

fn local_display_name() -> String {
    format!("{}@{}", local_username(), local_hostname())
}

fn local_username() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn local_hostname() -> String {
    gethostname::gethostname()
        .into_string()
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn sanitize_identifier(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}
