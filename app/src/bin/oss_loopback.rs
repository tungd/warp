use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{prelude::BASE64_URL_SAFE, Engine as _};
use prost::Message as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{runtime::Runtime, sync::mpsc};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};
use warp_multi_agent_api as maa;

const LOCAL_ACCOUNT_FILE: &str = "local-account.json";
const LOCAL_LLM_FILE: &str = "llm.toml";
const LOCAL_AGENT_MAX_TURNS: usize = 8;
const LOCAL_TOOL_DEFAULT_COMMAND_TIMEOUT_SECS: usize = 30;
const LOCAL_TOOL_DEFAULT_GREP_MATCHES: usize = 100;
const LOCAL_TOOL_MAX_COMMAND_OUTPUT_BYTES: usize = 64_000;
const LOCAL_TOOL_MAX_COMMAND_TIMEOUT_SECS: usize = 120;
const LOCAL_TOOL_MAX_GREP_BYTES: usize = 64_000;
const LOCAL_TOOL_MAX_GREP_MATCHES: usize = 1_000;
const LOCAL_TOOL_MAX_READ_BYTES: usize = 64_000;
const LOCAL_TOOL_MAX_WRITE_BYTES: usize = 64_000;
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
    active_model: String,
    #[serde(default)]
    providers: Vec<LocalLlmProviderConfig>,
    #[serde(default)]
    models: Vec<LocalLlmConfigModel>,
}

impl LocalLlmConfig {
    fn load() -> Result<Self> {
        let path = llm_config_path();
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }

        fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .and_then(|contents| {
                toml::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))
            })
    }

    fn models(&self) -> Result<Vec<ResolvedLocalLlm>> {
        self.models
            .iter()
            .map(|model| self.resolve_model(model))
            .collect()
    }

    fn active_model(&self) -> Result<ResolvedLocalLlm> {
        let active_model = required_str(&self.active_model, "active_model")?;
        let model = self
            .models
            .iter()
            .find(|model| model.matches(active_model))
            .with_context(|| format!("active_model '{active_model}' was not found in models"))?;
        self.resolve_model(model)
    }

    fn resolve_model(&self, model: &LocalLlmConfigModel) -> Result<ResolvedLocalLlm> {
        let provider_name = required_str(&model.provider, "model.provider")?;
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.name.trim() == provider_name)
            .with_context(|| {
                format!(
                    "provider '{provider_name}' for model '{}' was not found",
                    model.alias_or_name()
                )
            })?;

        let model_name = required_str(&model.name, "model.name")?;
        let alias = model.alias_or_name();
        let api_base = required_str(&provider.api_base, "provider.api_base")?;
        let api_style = provider
            .api_style
            .as_deref()
            .map(str::trim)
            .filter(|api_style| !api_style.is_empty())
            .unwrap_or("openai");
        let token = provider.resolve_api_key().with_context(|| {
            format!(
                "provider '{}' requires api_key, token, or api_key_env_var with a value",
                provider.name
            )
        })?;

        Ok(ResolvedLocalLlm {
            id: model
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .unwrap_or(alias)
                .to_owned(),
            display_name: model
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|display_name| !display_name.is_empty())
                .unwrap_or(alias)
                .to_owned(),
            base_model_name: model_name.to_owned(),
            base_url: api_base.to_owned(),
            api_style: api_style.to_owned(),
            token,
            token_configured: true,
            headers: provider.resolved_headers(),
            description: model
                .description
                .as_deref()
                .or(provider.description.as_deref())
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(ToOwned::to_owned),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmProviderConfig {
    name: String,
    api_base: String,
    api_key_env_var: Option<String>,
    api_key: Option<String>,
    token: Option<String>,
    api_style: Option<String>,
    description: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

impl LocalLlmProviderConfig {
    fn resolve_api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .or(self.token.as_deref())
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.api_key_env_var
                    .as_deref()
                    .map(str::trim)
                    .filter(|env_var| !env_var.is_empty())
                    .and_then(|env_var| std::env::var(env_var).ok())
                    .map(|token| token.trim().to_owned())
                    .filter(|token| !token.is_empty())
            })
    }

    fn resolved_headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            .filter(|(name, value)| !name.is_empty() && !value.is_empty())
            .collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmConfigModel {
    name: String,
    provider: String,
    alias: Option<String>,
    id: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
}

impl LocalLlmConfigModel {
    fn alias_or_name(&self) -> &str {
        self.alias
            .as_deref()
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .or_else(|| Some(self.name.trim()).filter(|name| !name.is_empty()))
            .unwrap_or("unknown")
    }

    fn matches(&self, active_model: &str) -> bool {
        self.alias
            .as_deref()
            .is_some_and(|alias| alias.trim() == active_model)
            || self.name.trim() == active_model
            || self
                .id
                .as_deref()
                .is_some_and(|id| id.trim() == active_model)
    }
}

#[derive(Clone, Debug)]
struct ResolvedLocalLlm {
    id: String,
    display_name: String,
    base_model_name: String,
    base_url: String,
    api_style: String,
    token: String,
    token_configured: bool,
    headers: Vec<(String, String)>,
    description: Option<String>,
}

impl ResolvedLocalLlm {
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
        self.description
            .clone()
            .unwrap_or_else(|| format!("Local {}", self.api_style_label()))
    }

    fn api_style_label(&self) -> &str {
        match self.api_style.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => "Anthropic",
            "google" | "gemini" => "Google",
            "openai" | "openai-compatible" | "openai_compatible" => "OpenAI",
            "xai" | "grok" => "xAI",
            "openrouter" => "OpenRouter",
            _ => self.api_style.trim(),
        }
    }
}

#[derive(Clone)]
struct ServerState {
    account: Arc<LocalAccount>,
    client: reqwest::Client,
}

pub struct LoopbackServer {
    _runtime: Runtime,
    server_root_url: String,
}

impl LoopbackServer {
    pub fn spawn() -> Result<Self> {
        let account = Arc::new(LocalAccount::load_or_create()?);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to create OSS loopback HTTP client")?;
        let state = ServerState { account, client };

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
            .route("/ai/multi-agent", post(multi_agent))
            .route("/ai/passive-suggestions", post(passive_suggestions))
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

async fn multi_agent(State(state): State<ServerState>, body: Bytes) -> Response {
    let request = match maa::Request::decode(body) {
        Ok(request) => request,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "errors": [{
                        "message": format!("invalid multi-agent protobuf request: {err}")
                    }]
                })),
            )
                .into_response()
        }
    };

    multi_agent_response_event_stream(state, request)
}

async fn passive_suggestions(_: State<ServerState>, body: Bytes) -> Response {
    let request =
        match maa::Request::decode(body) {
            Ok(request) => request,
            Err(err) => return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "errors": [{
                        "message": format!("invalid passive-suggestions protobuf request: {err}")
                    }]
                })),
            )
                .into_response(),
        };

    response_event_stream(finished_response_events(&request))
}

async fn generate_local_agent_output_with_progress<OnToolCall, OnToolResult>(
    state: &ServerState,
    request: &maa::Request,
    mut on_tool_call: OnToolCall,
    mut on_tool_result: OnToolResult,
) -> Result<LocalAgentRun>
where
    OnToolCall: FnMut(&LocalToolCall) + Send,
    OnToolResult: FnMut(&LocalToolEvent) + Send,
{
    let model = LocalLlmConfig::load()?.active_model()?;

    match model.api_style.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => {
            let prompt = extract_user_prompt(request)
                .filter(|prompt| !prompt.trim().is_empty())
                .unwrap_or_else(|| "Continue the current Warp agent conversation.".to_string());
            call_anthropic_compatible(&state.client, &model, &prompt)
                .await
                .map(LocalAgentRun::from_output)
        }
        "openai" | "openai-compatible" | "openai_compatible" | "xai" | "grok" | "google"
        | "gemini" | "openrouter" => {
            let workspace = workspace_for_request(request);
            let messages = openai_messages_for_request(request);
            call_openai_compatible_with_progress(
                &state.client,
                &model,
                messages,
                &workspace,
                &mut on_tool_call,
                &mut on_tool_result,
            )
            .await
        }
        _ => {
            let workspace = workspace_for_request(request);
            let messages = openai_messages_for_request(request);
            call_openai_compatible_with_progress(
                &state.client,
                &model,
                messages,
                &workspace,
                &mut on_tool_call,
                &mut on_tool_result,
            )
            .await
        }
    }
}

#[allow(deprecated)]
fn workspace_for_request(request: &maa::Request) -> PathBuf {
    workspace_from_input_context(
        request
            .input
            .as_ref()
            .and_then(|input| input.context.as_ref()),
    )
    .or_else(|| workspace_from_task_context(request))
    .unwrap_or_else(default_workspace)
}

fn workspace_from_input_context(context: Option<&maa::InputContext>) -> Option<PathBuf> {
    context
        .and_then(|context| context.directory.as_ref())
        .and_then(|directory| path_from_pwd(&directory.pwd))
}

fn workspace_from_task_context(request: &maa::Request) -> Option<PathBuf> {
    request
        .task_context
        .as_ref()?
        .tasks
        .iter()
        .find_map(|task| {
            task.messages
                .iter()
                .rev()
                .find_map(|message| match message.message.as_ref()? {
                    maa::message::Message::UserQuery(query) => {
                        workspace_from_input_context(query.context.as_ref())
                    }
                    _ => None,
                })
        })
}

fn path_from_pwd(pwd: &str) -> Option<PathBuf> {
    let pwd = pwd.trim();
    (!pwd.is_empty()).then(|| PathBuf::from(pwd))
}

fn default_workspace() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
async fn call_openai_compatible(
    client: &reqwest::Client,
    model: &ResolvedLocalLlm,
    messages: Vec<Value>,
    workspace: &Path,
) -> Result<LocalAgentRun> {
    call_openai_compatible_with_progress(client, model, messages, workspace, |_| {}, |_| {}).await
}

async fn call_openai_compatible_with_progress<OnToolCall, OnToolResult>(
    client: &reqwest::Client,
    model: &ResolvedLocalLlm,
    messages: Vec<Value>,
    workspace: &Path,
    on_tool_call: OnToolCall,
    on_tool_result: OnToolResult,
) -> Result<LocalAgentRun>
where
    OnToolCall: FnMut(&LocalToolCall) + Send,
    OnToolResult: FnMut(&LocalToolEvent) + Send,
{
    run_openai_agent_loop_with_progress(
        messages,
        workspace,
        |messages| call_openai_chat_completion(client, model, messages),
        on_tool_call,
        on_tool_result,
    )
    .await
}

async fn call_openai_chat_completion(
    client: &reqwest::Client,
    model: &ResolvedLocalLlm,
    messages: Vec<Value>,
) -> Result<Value> {
    let url = completion_url(&model.base_url, "chat/completions");
    let mut request = client.post(url).json(&openai_chat_completion_payload(
        &model.base_model_name,
        messages,
    ));
    request = apply_configured_headers(request, model)?;
    request = request.bearer_auth(&model.token);

    let response = request.send().await.context("failed to call local LLM")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read local LLM response")?;
    if !status.is_success() {
        anyhow::bail!("local LLM returned {status}: {body}");
    }

    serde_json::from_str(&body).context("failed to parse local LLM response")
}

fn openai_chat_completion_payload(model_name: &str, messages: Vec<Value>) -> Value {
    json!({
        "model": model_name,
        "messages": messages,
        "tools": local_openai_tools(),
        "tool_choice": "auto",
        "stream": false,
    })
}

fn local_openai_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a UTF-8 text file from the current workspace. The path must be relative to the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Zero-based line offset to start reading from"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read"
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a UTF-8 text file in the current workspace. The path must be relative to the workspace. Existing files require overwrite=true.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "Complete UTF-8 file contents"
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "Set to true to replace an existing file"
                        }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "search_replace",
                "description": "Make a targeted edit in an existing UTF-8 file by replacing an exact text block. The search text must match exactly once.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path to edit"
                        },
                        "search": {
                            "type": "string",
                            "description": "Exact text to replace. It must appear exactly once in the file."
                        },
                        "replace": {
                            "type": "string",
                            "description": "Replacement text"
                        }
                    },
                    "required": ["path", "search", "replace"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search workspace files for a Rust-regex pattern. The path must be relative to the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regular expression to search for"
                        },
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file or directory to search. Defaults to the workspace root."
                        },
                        "max_matches": {
                            "type": "integer",
                            "description": "Maximum number of matching lines to return"
                        }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a non-interactive shell command in the current workspace. Prefer read_file, grep, and write_file for file operations.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to run"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Timeout in seconds. Defaults to 30 and is capped at 120."
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        }),
    ]
}

async fn call_anthropic_compatible(
    client: &reqwest::Client,
    model: &ResolvedLocalLlm,
    prompt: &str,
) -> Result<String> {
    let url = completion_url(&model.base_url, "v1/messages");
    let mut request = client
        .post(url)
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
        )
        .json(&json!({
            "model": model.base_model_name,
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": prompt
            }],
        }));
    request = apply_configured_headers(request, model)?;
    request = request.header("x-api-key", &model.token);

    let response = request.send().await.context("failed to call local LLM")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read local LLM response")?;
    if !status.is_success() {
        anyhow::bail!("local LLM returned {status}: {body}");
    }

    let value: Value = serde_json::from_str(&body).context("failed to parse local LLM response")?;
    extract_anthropic_text(&value).context("local LLM response did not contain text content")
}

fn completion_url(base_url: &str, endpoint: &str) -> String {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.ends_with(endpoint) {
        base_url.to_string()
    } else {
        format!("{base_url}/{endpoint}")
    }
}

fn apply_configured_headers(
    mut request: reqwest::RequestBuilder,
    model: &ResolvedLocalLlm,
) -> Result<reqwest::RequestBuilder> {
    for (name, value) in &model.headers {
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid configured header name: {name}"))?;
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .with_context(|| format!("invalid configured header value for {name}"))?;
        request = request.header(header_name, header_value);
    }
    Ok(request)
}

#[derive(Clone, Debug, PartialEq)]
struct LocalAssistantTurn {
    content: String,
    tool_calls: Vec<LocalToolCall>,
}

#[derive(Clone, Debug, PartialEq)]
struct LocalToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
struct LocalToolResult {
    tool_call_id: String,
    name: String,
    content: String,
}

#[derive(Clone, Debug, PartialEq)]
struct LocalToolEvent {
    tool_call: LocalToolCall,
    result: LocalToolResult,
}

#[derive(Clone, Debug, PartialEq)]
struct LocalAgentRun {
    output: String,
    tool_events: Vec<LocalToolEvent>,
}

impl LocalAgentRun {
    fn from_output(output: String) -> Self {
        Self {
            output,
            tool_events: Vec::new(),
        }
    }
}

fn execute_local_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    match tool_call.name.as_str() {
        "read_file" => execute_read_file_tool(tool_call, workspace),
        "write_file" => execute_write_file_tool(tool_call, workspace),
        "search_replace" => execute_search_replace_tool(tool_call, workspace),
        "grep" => execute_grep_tool(tool_call, workspace),
        "bash" => execute_bash_tool(tool_call, workspace),
        name => anyhow::bail!("unsupported local tool: {name}"),
    }
}

fn execute_read_file_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    let path = tool_call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .context("read_file requires a non-empty path")?;
    let offset = optional_usize_arg(&tool_call.arguments, "offset")?.unwrap_or(0);
    let limit = optional_usize_arg(&tool_call.arguments, "limit")?;
    if limit.is_some_and(|limit| limit == 0) {
        anyhow::bail!("limit must be a positive integer");
    }

    let resolved = resolve_workspace_path(workspace, path)?;
    let content = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    let (content, was_truncated) = select_file_lines(&content, offset, limit);
    let content = if was_truncated {
        format!("{content}\n[read_file output truncated]\n")
    } else {
        content
    };

    Ok(LocalToolResult {
        tool_call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content,
    })
}

fn select_file_lines(content: &str, offset: usize, limit: Option<usize>) -> (String, bool) {
    let mut selected = String::new();
    let mut lines_read = 0usize;
    let mut was_truncated = false;

    for (index, line) in content.split_inclusive('\n').enumerate() {
        if index < offset {
            continue;
        }
        if limit.is_some_and(|limit| lines_read >= limit) {
            break;
        }
        if selected.len() + line.len() > LOCAL_TOOL_MAX_READ_BYTES {
            was_truncated = true;
            break;
        }
        selected.push_str(line);
        lines_read += 1;
    }

    (selected, was_truncated)
}

fn execute_write_file_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    let path = tool_call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .context("write_file requires a non-empty path")?;
    let content = tool_call
        .arguments
        .get("content")
        .and_then(Value::as_str)
        .context("write_file requires content")?;
    let overwrite = tool_call
        .arguments
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content_bytes = content.len();
    if content_bytes > LOCAL_TOOL_MAX_WRITE_BYTES {
        anyhow::bail!("write_file content exceeds {LOCAL_TOOL_MAX_WRITE_BYTES} bytes");
    }

    let resolved = resolve_workspace_path(workspace, path)?;
    let file_existed = resolved.exists();
    if file_existed && !overwrite {
        anyhow::bail!("file exists at {path}; set overwrite=true to replace it");
    }
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&resolved, content)
        .with_context(|| format!("failed to write {}", resolved.display()))?;

    let action = if file_existed { "Overwrote" } else { "Wrote" };
    Ok(LocalToolResult {
        tool_call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content: format!("{action} {content_bytes} bytes to {path}."),
    })
}

fn execute_search_replace_tool(
    tool_call: &LocalToolCall,
    workspace: &Path,
) -> Result<LocalToolResult> {
    let path = tool_call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .context("search_replace requires a non-empty path")?;
    let search = tool_call
        .arguments
        .get("search")
        .and_then(Value::as_str)
        .filter(|search| !search.is_empty())
        .context("search_replace requires non-empty search text")?;
    let replace = tool_call
        .arguments
        .get("replace")
        .and_then(Value::as_str)
        .context("search_replace requires replacement text")?;

    let resolved = resolve_workspace_path(workspace, path)?;
    let content = fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    let match_count = content.match_indices(search).take(2).count();
    if match_count != 1 {
        anyhow::bail!("search_replace requires exactly one match, found {match_count}");
    }

    let updated = content.replacen(search, replace, 1);
    if updated.len() > LOCAL_TOOL_MAX_WRITE_BYTES {
        anyhow::bail!("search_replace result exceeds {LOCAL_TOOL_MAX_WRITE_BYTES} bytes");
    }
    fs::write(&resolved, updated)
        .with_context(|| format!("failed to write {}", resolved.display()))?;

    Ok(LocalToolResult {
        tool_call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content: format!("Replaced 1 occurrence in {path}."),
    })
}

fn execute_grep_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    let pattern = tool_call
        .arguments
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .context("grep requires a non-empty pattern")?;
    let path = tool_call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    let max_matches = optional_usize_arg(&tool_call.arguments, "max_matches")?
        .unwrap_or(LOCAL_TOOL_DEFAULT_GREP_MATCHES)
        .clamp(1, LOCAL_TOOL_MAX_GREP_MATCHES);
    let regex = Regex::new(pattern).with_context(|| format!("invalid grep pattern: {pattern}"))?;
    let root = resolve_workspace_path(workspace, path)?;
    if !root.exists() {
        anyhow::bail!("grep path does not exist: {path}");
    }

    let mut content = String::new();
    let mut match_count = 0usize;
    let mut was_truncated = false;

    'entries: for entry in WalkDir::new(&root)
        .into_iter()
        .filter_entry(should_visit_grep_entry)
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let file_content = match fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for (index, line) in file_content.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }

            let relative_path = entry.path().strip_prefix(workspace).unwrap_or(entry.path());
            let rendered = format!(
                "{}:{}:{}\n",
                relative_path.display(),
                index + 1,
                line.trim_end_matches('\r')
            );
            if content.len() + rendered.len() > LOCAL_TOOL_MAX_GREP_BYTES {
                was_truncated = true;
                break 'entries;
            }
            content.push_str(&rendered);
            match_count += 1;
            if match_count >= max_matches {
                was_truncated = true;
                break 'entries;
            }
        }
    }

    if match_count == 0 {
        content.push_str("No matches.\n");
    } else if was_truncated {
        content.push_str("Search results truncated.\n");
    }

    Ok(LocalToolResult {
        tool_call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content,
    })
}

fn should_visit_grep_entry(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | "node_modules" | "target" | ".venv" | "venv")
    )
}

fn optional_usize_arg(arguments: &Value, name: &str) -> Result<Option<usize>> {
    match arguments.get(name) {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(|value| value as usize)
            .map(Some)
            .with_context(|| format!("{name} must be a positive integer")),
        Some(_) => anyhow::bail!("{name} must be a positive integer"),
    }
}

struct LocalCommandOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn execute_bash_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    let command = tool_call
        .arguments
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .context("bash requires a non-empty command")?;
    let timeout_secs = optional_usize_arg(&tool_call.arguments, "timeout_secs")?
        .unwrap_or(LOCAL_TOOL_DEFAULT_COMMAND_TIMEOUT_SECS)
        .clamp(1, LOCAL_TOOL_MAX_COMMAND_TIMEOUT_SECS);
    let output =
        run_local_shell_command(workspace, command, Duration::from_secs(timeout_secs as u64))?;

    Ok(LocalToolResult {
        tool_call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content: format_command_output(command, &output),
    })
}

fn run_local_shell_command(
    workspace: &Path,
    command: &str,
    timeout: Duration,
) -> Result<LocalCommandOutput> {
    let temp_id = Uuid::new_v4();
    let stdout_path = std::env::temp_dir().join(format!("warp-oss-stdout-{temp_id}.txt"));
    let stderr_path = std::env::temp_dir().join(format!("warp-oss-stderr-{temp_id}.txt"));
    let stdout_file = fs::File::create(&stdout_path)
        .with_context(|| format!("failed to create {}", stdout_path.display()))?;
    let stderr_file = fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let mut child = std::process::Command::new(shell)
        .arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .env("CI", "true")
        .env("NONINTERACTIVE", "1")
        .env("NO_TTY", "1")
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .context("failed to run bash command")?;

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("failed to poll bash command")? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child
                .wait()
                .context("failed to wait for killed bash command")?;
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = read_capped_command_output(&stdout_path)?;
    let stderr = read_capped_command_output(&stderr_path)?;
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);

    Ok(LocalCommandOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
    })
}

fn read_capped_command_output(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let was_truncated = bytes.len() > LOCAL_TOOL_MAX_COMMAND_OUTPUT_BYTES;
    let bytes = if was_truncated {
        &bytes[..LOCAL_TOOL_MAX_COMMAND_OUTPUT_BYTES]
    } else {
        &bytes
    };
    let mut output = String::from_utf8_lossy(bytes).into_owned();
    if was_truncated {
        output.push_str("\n[command output truncated]\n");
    }
    Ok(output)
}

fn format_command_output(command: &str, output: &LocalCommandOutput) -> String {
    let exit_code = output
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string());
    let mut result = format!("Command: {command}\nExit code: {exit_code}\n");
    if output.timed_out {
        result.push_str("Timed out: true\n");
    }
    if !output.stdout.is_empty() {
        result.push_str("\nStdout:\n");
        result.push_str(&output.stdout);
        if !output.stdout.ends_with('\n') {
            result.push('\n');
        }
    }
    if !output.stderr.is_empty() {
        result.push_str("\nStderr:\n");
        result.push_str(&output.stderr);
        if !output.stderr.ends_with('\n') {
            result.push('\n');
        }
    }
    result
}

fn resolve_workspace_path(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let requested = Path::new(requested);
    if requested.is_absolute()
        || requested
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("path must stay inside the workspace");
    }
    Ok(workspace.join(requested))
}

fn parse_openai_assistant_turn(value: &Value) -> Result<LocalAssistantTurn> {
    let message = value
        .pointer("/choices/0/message")
        .or_else(|| value.pointer("/message"))
        .context("local LLM response did not contain an assistant message")?;
    let content = message
        .get("content")
        .and_then(text_value)
        .unwrap_or_default();
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|tool_calls| {
            tool_calls
                .iter()
                .enumerate()
                .map(parse_openai_tool_call)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(LocalAssistantTurn {
        content,
        tool_calls,
    })
}

fn parse_openai_tool_call((index, tool_call): (usize, &Value)) -> Result<LocalToolCall> {
    let function = tool_call
        .get("function")
        .context("tool call did not contain function data")?;
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("local-tool-call-{index}"));
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .context("tool call function name is required")?
        .to_owned();
    let arguments = match function.get("arguments") {
        Some(Value::String(arguments)) if !arguments.trim().is_empty() => {
            serde_json::from_str(arguments)
                .with_context(|| format!("tool call '{name}' arguments were not valid JSON"))?
        }
        Some(arguments) => arguments.clone(),
        None => json!({}),
    };

    Ok(LocalToolCall {
        id,
        name,
        arguments,
    })
}

fn extract_anthropic_text(value: &Value) -> Option<String> {
    value.get("content").and_then(text_value)
}

fn text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.to_owned()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| item.as_str())
                })
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn openai_messages_for_request(request: &maa::Request) -> Vec<Value> {
    let mut messages = vec![openai_system_message()];
    let mut local_tool_call_ids = std::collections::HashSet::new();

    if let Some(task_context) = request.task_context.as_ref() {
        for task in &task_context.tasks {
            for message in &task.messages {
                match message.message.as_ref() {
                    Some(maa::message::Message::UserQuery(query)) => {
                        if !query.query.trim().is_empty() {
                            messages.push(openai_user_message(&query.query));
                        }
                    }
                    Some(maa::message::Message::AgentOutput(output)) => {
                        if !output.text.trim().is_empty() {
                            messages.push(openai_assistant_text_message(&output.text));
                        }
                    }
                    Some(maa::message::Message::ToolCall(tool_call)) => {
                        if let Some(local_tool_call) = local_tool_call_from_api(tool_call) {
                            local_tool_call_ids.insert(local_tool_call.id.clone());
                            messages.push(openai_assistant_message(&LocalAssistantTurn {
                                content: String::new(),
                                tool_calls: vec![local_tool_call],
                            }));
                        }
                    }
                    Some(maa::message::Message::ToolCallResult(result))
                        if local_tool_call_ids.contains(&result.tool_call_id) =>
                    {
                        messages.push(openai_tool_result_message(&LocalToolResult {
                            tool_call_id: result.tool_call_id.clone(),
                            name: "tool".to_string(),
                            content: api_tool_result_text(result),
                        }));
                    }
                    _ => {}
                }
            }
        }
    }

    let prompt = extract_user_prompt(request)
        .filter(|prompt| !prompt.trim().is_empty())
        .unwrap_or_else(|| "Continue the current Warp agent conversation.".to_string());
    messages.push(openai_user_message(&prompt));
    messages
}

#[cfg(test)]
fn openai_messages_from_prompt(prompt: &str) -> Vec<Value> {
    vec![openai_system_message(), openai_user_message(prompt)]
}

fn openai_system_message() -> Value {
    json!({
        "role": "system",
        "content": local_agent_system_prompt()
    })
}

fn openai_user_message(content: &str) -> Value {
    json!({
        "role": "user",
        "content": content
    })
}

fn openai_assistant_text_message(content: &str) -> Value {
    json!({
        "role": "assistant",
        "content": content
    })
}

fn local_tool_call_from_api(tool_call: &maa::message::ToolCall) -> Option<LocalToolCall> {
    let tool = tool_call.tool.as_ref()?;
    let (name, arguments) = match tool {
        maa::message::tool_call::Tool::RunShellCommand(command) => (
            "bash",
            json!({
                "command": command.command
            }),
        ),
        maa::message::tool_call::Tool::ReadFiles(read_files) => {
            let file = read_files.files.first()?;
            let mut arguments = json!({
                "path": file.name
            });
            if let Some(range) = file.line_ranges.first() {
                arguments["offset"] = json!(range.start.saturating_sub(1));
                if range.end >= range.start {
                    arguments["limit"] = json!(range.end - range.start + 1);
                }
            }
            ("read_file", arguments)
        }
        maa::message::tool_call::Tool::Grep(grep) => (
            "grep",
            json!({
                "pattern": grep.queries.first()?,
                "path": if grep.path.is_empty() { "." } else { grep.path.as_str() }
            }),
        ),
        maa::message::tool_call::Tool::ApplyFileDiffs(diffs) => {
            if let Some(diff) = diffs.diffs.first() {
                (
                    "search_replace",
                    json!({
                        "path": diff.file_path,
                        "search": diff.search,
                        "replace": diff.replace
                    }),
                )
            } else if let Some(new_file) = diffs.new_files.first() {
                (
                    "write_file",
                    json!({
                        "path": new_file.file_path,
                        "content": new_file.content,
                        "overwrite": true
                    }),
                )
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some(LocalToolCall {
        id: tool_call.tool_call_id.clone(),
        name: name.to_string(),
        arguments,
    })
}

fn api_tool_result_text(result: &maa::message::ToolCallResult) -> String {
    match result.result.as_ref() {
        Some(maa::message::tool_call_result::Result::RunShellCommand(result)) => {
            match result.result.as_ref() {
                Some(maa::run_shell_command_result::Result::CommandFinished(finished)) => format!(
                    "Command: {}\nExit code: {}\n\n{}",
                    result.command,
                    finished.exit_code,
                    strip_local_command_header(&finished.output)
                ),
                _ => format!("Command result for {} is unavailable.", result.command),
            }
        }
        Some(maa::message::tool_call_result::Result::ReadFiles(result)) => {
            match result.result.as_ref() {
                Some(maa::read_files_result::Result::TextFilesSuccess(success)) => success
                    .files
                    .iter()
                    .map(|file| format!("{}:\n{}", file.file_path, file.content))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                Some(maa::read_files_result::Result::AnyFilesSuccess(success)) => {
                    format!("Read {} file(s).", success.files.len())
                }
                Some(maa::read_files_result::Result::Error(error)) => error.message.clone(),
                None => "Read file result is unavailable.".to_string(),
            }
        }
        Some(maa::message::tool_call_result::Result::Grep(result)) => {
            match result.result.as_ref() {
                Some(maa::grep_result::Result::Success(success)) => success
                    .matched_files
                    .iter()
                    .map(|file| {
                        let lines = file
                            .matched_lines
                            .iter()
                            .map(|line| line.line_number.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{}: {}", file.file_path, lines)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(maa::grep_result::Result::Error(error)) => error.message.clone(),
                None => "Grep result is unavailable.".to_string(),
            }
        }
        Some(maa::message::tool_call_result::Result::ApplyFileDiffs(result)) => {
            match result.result.as_ref() {
                Some(maa::apply_file_diffs_result::Result::Success(success)) => {
                    apply_file_diffs_result_text(success)
                }
                Some(maa::apply_file_diffs_result::Result::Error(error)) => error.message.clone(),
                None => "File edit result is unavailable.".to_string(),
            }
        }
        _ => "Tool result is unavailable.".to_string(),
    }
}

fn strip_local_command_header(output: &str) -> String {
    let mut lines = output.lines();
    if lines
        .next()
        .is_some_and(|line| line.starts_with("Command: "))
        && lines
            .next()
            .is_some_and(|line| line.starts_with("Exit code: "))
    {
        let stripped = lines.collect::<Vec<_>>().join("\n");
        stripped
            .strip_prefix('\n')
            .unwrap_or(&stripped)
            .trim_end_matches('\n')
            .to_string()
    } else {
        output.to_string()
    }
}

fn apply_file_diffs_result_text(success: &maa::apply_file_diffs_result::Success) -> String {
    let mut parts = Vec::new();

    for updated in &success.updated_files_v2 {
        if let Some(file) = updated.file.as_ref() {
            parts.push(format!("{}:\n{}", file.file_path, file.content));
        }
    }
    if !success.deleted_files.is_empty() {
        let deleted = success
            .deleted_files
            .iter()
            .map(|file| file.file_path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("Deleted files: {deleted}"));
    }

    if parts.is_empty() {
        format!(
            "Applied file edits to {} file(s).",
            success.updated_files_v2.len() + success.deleted_files.len()
        )
    } else {
        parts.join("\n\n")
    }
}

#[cfg(test)]
async fn run_openai_agent_loop<F, Fut>(
    messages: Vec<Value>,
    workspace: &Path,
    complete: F,
) -> Result<LocalAgentRun>
where
    F: FnMut(Vec<Value>) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    run_openai_agent_loop_with_progress(messages, workspace, complete, |_| {}, |_| {}).await
}

async fn run_openai_agent_loop_with_progress<F, Fut, OnToolCall, OnToolResult>(
    mut messages: Vec<Value>,
    workspace: &Path,
    mut complete: F,
    mut on_tool_call: OnToolCall,
    mut on_tool_result: OnToolResult,
) -> Result<LocalAgentRun>
where
    F: FnMut(Vec<Value>) -> Fut,
    Fut: Future<Output = Result<Value>>,
    OnToolCall: FnMut(&LocalToolCall) + Send,
    OnToolResult: FnMut(&LocalToolEvent) + Send,
{
    let mut tool_events = Vec::new();
    for _ in 0..LOCAL_AGENT_MAX_TURNS {
        let response = complete(messages.clone()).await?;
        let turn = parse_openai_assistant_turn(&response)?;
        if turn.tool_calls.is_empty() {
            return Ok(LocalAgentRun {
                output: turn.content,
                tool_events,
            });
        }

        messages.push(openai_assistant_message(&turn));
        for tool_call in &turn.tool_calls {
            on_tool_call(tool_call);
            let result =
                execute_local_tool(tool_call, workspace).unwrap_or_else(|err| LocalToolResult {
                    tool_call_id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    content: format!("Tool failed: {err:#}"),
                });
            messages.push(openai_tool_result_message(&result));
            let event = LocalToolEvent {
                tool_call: tool_call.clone(),
                result,
            };
            on_tool_result(&event);
            tool_events.push(event);
        }
    }

    anyhow::bail!("local agent exceeded {LOCAL_AGENT_MAX_TURNS} tool turns")
}

fn local_agent_system_prompt() -> &'static str {
    "You are a local coding agent running inside a Warp OSS loopback sidecar. \
Use tools to inspect and modify the user's current workspace. \
Available tools: read_file for reading workspace files, grep for searching, search_replace for targeted exact-match edits, write_file for creating or overwriting files, and bash for non-interactive workspace commands. \
Before editing an existing file, inspect it with read_file or grep. \
Prefer read_file, grep, search_replace, and write_file over bash for file operations. \
After making code changes, run a relevant verification command with bash when one is reasonably available. \
Keep final answers concise and report what changed plus any verification result."
}

fn openai_assistant_message(turn: &LocalAssistantTurn) -> Value {
    json!({
        "role": "assistant",
        "content": turn.content,
        "tool_calls": turn.tool_calls.iter().map(openai_tool_call_message).collect::<Vec<_>>(),
    })
}

fn openai_tool_call_message(tool_call: &LocalToolCall) -> Value {
    json!({
        "id": tool_call.id,
        "type": "function",
        "function": {
            "name": tool_call.name,
            "arguments": tool_call.arguments.to_string(),
        }
    })
}

fn openai_tool_result_message(result: &LocalToolResult) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": result.tool_call_id,
        "name": result.name,
        "content": result.content,
    })
}

fn agent_output_message(output: &str, task_id: &str, request_id: &str) -> maa::Message {
    local_message(
        task_id,
        request_id,
        maa::message::Message::AgentOutput(maa::message::AgentOutput {
            text: output.to_string(),
        }),
    )
}

fn local_tool_call_message(
    tool_call: &LocalToolCall,
    task_id: &str,
    request_id: &str,
) -> maa::Message {
    local_message(
        task_id,
        request_id,
        maa::message::Message::ToolCall(maa::message::ToolCall {
            tool_call_id: tool_call.id.clone(),
            tool: Some(api_tool_from_local(tool_call)),
        }),
    )
}

fn local_tool_result_message(
    event: &LocalToolEvent,
    task_id: &str,
    request_id: &str,
) -> maa::Message {
    local_message(
        task_id,
        request_id,
        maa::message::Message::ToolCallResult(maa::message::ToolCallResult {
            tool_call_id: event.result.tool_call_id.clone(),
            context: None,
            result: Some(api_tool_result_from_local(event)),
        }),
    )
}

fn local_message(task_id: &str, request_id: &str, message: maa::message::Message) -> maa::Message {
    maa::Message {
        id: format!("local-message-{}", Uuid::new_v4()),
        task_id: task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: Some(now_timestamp()),
        server_message_data: String::new(),
        citations: Vec::new(),
        message: Some(message),
    }
}

fn api_tool_from_local(tool_call: &LocalToolCall) -> maa::message::tool_call::Tool {
    match tool_call.name.as_str() {
        "read_file" => maa::message::tool_call::Tool::ReadFiles(
            maa::message::tool_call::ReadFiles {
                files: vec![maa::message::tool_call::read_files::File {
                    name: local_arg_str(&tool_call.arguments, "path")
                        .unwrap_or_default()
                        .to_string(),
                    line_ranges: local_file_line_range(&tool_call.arguments)
                        .into_iter()
                        .collect(),
                }],
            },
        ),
        "write_file" => maa::message::tool_call::Tool::ApplyFileDiffs(
            maa::message::tool_call::ApplyFileDiffs {
                summary: format!(
                    "Write {}",
                    local_arg_str(&tool_call.arguments, "path").unwrap_or("file")
                ),
                diffs: Vec::new(),
                new_files: vec![maa::message::tool_call::apply_file_diffs::NewFile {
                    file_path: local_arg_str(&tool_call.arguments, "path")
                        .unwrap_or_default()
                        .to_string(),
                    content: local_arg_str(&tool_call.arguments, "content")
                        .unwrap_or_default()
                        .to_string(),
                }],
                deleted_files: Vec::new(),
                v4a_updates: Vec::new(),
            },
        ),
        "search_replace" => maa::message::tool_call::Tool::ApplyFileDiffs(
            maa::message::tool_call::ApplyFileDiffs {
                summary: format!(
                    "Replace text in {}",
                    local_arg_str(&tool_call.arguments, "path").unwrap_or("file")
                ),
                diffs: vec![maa::message::tool_call::apply_file_diffs::FileDiff {
                    file_path: local_arg_str(&tool_call.arguments, "path")
                        .unwrap_or_default()
                        .to_string(),
                    search: local_arg_str(&tool_call.arguments, "search")
                        .unwrap_or_default()
                        .to_string(),
                    replace: local_arg_str(&tool_call.arguments, "replace")
                        .unwrap_or_default()
                        .to_string(),
                }],
                new_files: Vec::new(),
                deleted_files: Vec::new(),
                v4a_updates: Vec::new(),
            },
        ),
        "grep" => maa::message::tool_call::Tool::Grep(maa::message::tool_call::Grep {
            queries: vec![local_arg_str(&tool_call.arguments, "pattern")
                .unwrap_or_default()
                .to_string()],
            path: local_arg_str(&tool_call.arguments, "path")
                .unwrap_or(".")
                .to_string(),
        }),
        "bash" => maa::message::tool_call::Tool::RunShellCommand(
            maa::message::tool_call::RunShellCommand {
                command: local_arg_str(&tool_call.arguments, "command")
                    .unwrap_or_default()
                    .to_string(),
                is_read_only: false,
                uses_pager: false,
                citations: Vec::new(),
                is_risky: false,
                risk_category: 0,
                wait_until_complete_value: Some(
                    maa::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                        true,
                    ),
                ),
            },
        ),
        _ => maa::message::tool_call::Tool::Server(maa::message::tool_call::Server {
            payload: json!({
                "name": tool_call.name,
                "arguments": tool_call.arguments,
            })
            .to_string(),
        }),
    }
}

fn api_tool_result_from_local(event: &LocalToolEvent) -> maa::message::tool_call_result::Result {
    let result = &event.result;
    match event.tool_call.name.as_str() {
        "read_file" => {
            maa::message::tool_call_result::Result::ReadFiles(api_read_file_result(event))
        }
        "write_file" | "search_replace" => maa::message::tool_call_result::Result::ApplyFileDiffs(
            api_apply_file_diffs_result(event),
        ),
        "grep" => maa::message::tool_call_result::Result::Grep(api_grep_result(event)),
        "bash" => {
            maa::message::tool_call_result::Result::RunShellCommand(api_run_shell_result(event))
        }
        _ => maa::message::tool_call_result::Result::Server(
            maa::message::tool_call_result::ServerResult {
                serialized_result: json!({
                    "name": result.name,
                    "content": result.content,
                })
                .to_string(),
            },
        ),
    }
}

fn api_read_file_result(event: &LocalToolEvent) -> maa::ReadFilesResult {
    if local_result_is_error(&event.result.content) {
        return maa::ReadFilesResult {
            result: Some(maa::read_files_result::Result::Error(
                maa::read_files_result::Error {
                    message: event.result.content.clone(),
                },
            )),
        };
    }

    maa::ReadFilesResult {
        result: Some(maa::read_files_result::Result::TextFilesSuccess(
            maa::read_files_result::TextFilesSuccess {
                files: vec![maa::FileContent {
                    file_path: local_arg_str(&event.tool_call.arguments, "path")
                        .unwrap_or_default()
                        .to_string(),
                    content: event.result.content.clone(),
                    line_range: local_file_line_range(&event.tool_call.arguments),
                }],
            },
        )),
    }
}

#[allow(deprecated)]
fn api_apply_file_diffs_result(event: &LocalToolEvent) -> maa::ApplyFileDiffsResult {
    if local_result_is_error(&event.result.content) {
        return maa::ApplyFileDiffsResult {
            result: Some(maa::apply_file_diffs_result::Result::Error(
                maa::apply_file_diffs_result::Error {
                    message: event.result.content.clone(),
                },
            )),
        };
    }

    let file_path = local_arg_str(&event.tool_call.arguments, "path")
        .unwrap_or_default()
        .to_string();
    let content = local_arg_str(&event.tool_call.arguments, "content")
        .unwrap_or(&event.result.content)
        .to_string();
    let updated_files_v2 = if file_path.is_empty() {
        Vec::new()
    } else {
        vec![maa::apply_file_diffs_result::success::UpdatedFileContent {
            file: Some(maa::FileContent {
                file_path,
                content,
                line_range: None,
            }),
            was_edited_by_user: false,
        }]
    };

    maa::ApplyFileDiffsResult {
        result: Some(maa::apply_file_diffs_result::Result::Success(
            maa::apply_file_diffs_result::Success {
                updated_files: Vec::new(),
                updated_files_v2,
                deleted_files: Vec::new(),
            },
        )),
    }
}

fn api_grep_result(event: &LocalToolEvent) -> maa::GrepResult {
    if local_result_is_error(&event.result.content) {
        return maa::GrepResult {
            result: Some(maa::grep_result::Result::Error(maa::grep_result::Error {
                message: event.result.content.clone(),
            })),
        };
    }

    maa::GrepResult {
        result: Some(maa::grep_result::Result::Success(
            maa::grep_result::Success {
                matched_files: grep_matches_from_content(&event.result.content),
            },
        )),
    }
}

#[allow(deprecated)]
fn api_run_shell_result(event: &LocalToolEvent) -> maa::RunShellCommandResult {
    let command = local_arg_str(&event.tool_call.arguments, "command")
        .unwrap_or_default()
        .to_string();
    let exit_code = shell_exit_code_from_content(&event.result.content);

    maa::RunShellCommandResult {
        command,
        output: event.result.content.clone(),
        exit_code,
        result: Some(maa::run_shell_command_result::Result::CommandFinished(
            maa::ShellCommandFinished {
                command_id: event.result.tool_call_id.clone(),
                output: event.result.content.clone(),
                exit_code,
            },
        )),
    }
}

fn local_arg_str<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn local_file_line_range(arguments: &Value) -> Option<maa::FileContentLineRange> {
    let limit = arguments.get("limit")?.as_u64()?;
    if limit == 0 {
        return None;
    }
    let offset = arguments
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    Some(maa::FileContentLineRange {
        start: (offset + 1).min(u32::MAX as u64) as u32,
        end: (offset + limit).min(u32::MAX as u64) as u32,
    })
}

fn local_result_is_error(content: &str) -> bool {
    content.starts_with("Tool failed:")
}

fn grep_matches_from_content(content: &str) -> Vec<maa::grep_result::success::GrepFileMatch> {
    let mut matches = BTreeMap::<String, Vec<u32>>::new();
    for line in content.lines() {
        if line == "No matches." || line == "Search results truncated." {
            continue;
        }
        let Some((path, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((line_number, _line_content)) = rest.split_once(':') else {
            continue;
        };
        let Ok(line_number) = line_number.parse::<u32>() else {
            continue;
        };
        matches
            .entry(path.to_string())
            .or_default()
            .push(line_number);
    }

    matches
        .into_iter()
        .map(
            |(file_path, matched_lines)| maa::grep_result::success::GrepFileMatch {
                file_path,
                matched_lines: matched_lines
                    .into_iter()
                    .map(
                        |line_number| maa::grep_result::success::grep_file_match::GrepLineMatch {
                            line_number,
                        },
                    )
                    .collect(),
            },
        )
        .collect()
}

fn shell_exit_code_from_content(content: &str) -> i32 {
    content
        .lines()
        .find_map(|line| line.strip_prefix("Exit code: "))
        .and_then(|exit_code| exit_code.trim().parse().ok())
        .unwrap_or_else(|| if local_result_is_error(content) { 1 } else { 0 })
}

fn multi_agent_response_event_stream(state: ServerState, request: maa::Request) -> Response {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let stream_ids = stream_ids(&request);
        let task_info = task_info(&request);
        send_response_event(&tx, init_event(&stream_ids));
        if task_info.needs_create {
            send_response_event(
                &tx,
                client_actions_event(vec![create_task_action(&task_info)]),
            );
        }

        let tool_call_tx = tx.clone();
        let tool_call_task_id = task_info.id.clone();
        let tool_call_request_id = stream_ids.request_id.clone();
        let tool_result_tx = tx.clone();
        let tool_result_task_id = task_info.id.clone();
        let tool_result_request_id = stream_ids.request_id.clone();

        let run = match generate_local_agent_output_with_progress(
            &state,
            &request,
            move |tool_call| {
                send_response_event(
                    &tool_call_tx,
                    add_messages_event(
                        &tool_call_task_id,
                        vec![local_tool_call_message(
                            tool_call,
                            &tool_call_task_id,
                            &tool_call_request_id,
                        )],
                    ),
                );
            },
            move |event| {
                send_response_event(
                    &tool_result_tx,
                    add_messages_event(
                        &tool_result_task_id,
                        vec![local_tool_result_message(
                            event,
                            &tool_result_task_id,
                            &tool_result_request_id,
                        )],
                    ),
                );
            },
        )
        .await
        {
            Ok(run) => run,
            Err(err) => local_agent_error_run(err),
        };

        send_response_event(
            &tx,
            add_messages_event(
                &task_info.id,
                vec![agent_output_message(
                    &run.output,
                    &task_info.id,
                    &stream_ids.request_id,
                )],
            ),
        );
        send_response_event(&tx, finished_event());
    });

    response_event_receiver_stream(rx)
}

#[cfg(test)]
fn agent_response_events(request: &maa::Request, run: LocalAgentRun) -> Vec<maa::ResponseEvent> {
    let stream_ids = stream_ids(request);
    let task_info = task_info(request);
    let mut messages = Vec::new();
    for event in &run.tool_events {
        messages.push(local_tool_call_message(
            &event.tool_call,
            &task_info.id,
            &stream_ids.request_id,
        ));
        messages.push(local_tool_result_message(
            event,
            &task_info.id,
            &stream_ids.request_id,
        ));
    }
    messages.push(agent_output_message(
        &run.output,
        &task_info.id,
        &stream_ids.request_id,
    ));

    let mut actions = Vec::new();
    if task_info.needs_create {
        actions.push(create_task_action(&task_info));
    }
    actions.push(add_messages_action(&task_info.id, messages));

    vec![
        init_event(&stream_ids),
        client_actions_event(actions),
        finished_event(),
    ]
}

fn create_task_action(task_info: &TaskInfo) -> maa::ClientAction {
    maa::ClientAction {
        action: Some(maa::client_action::Action::CreateTask(
            maa::client_action::CreateTask {
                task: Some(maa::Task {
                    id: task_info.id.clone(),
                    description: task_info.description.clone(),
                    dependencies: None,
                    messages: Vec::new(),
                    summary: String::new(),
                    server_data: String::new(),
                }),
            },
        )),
    }
}

fn add_messages_action(task_id: &str, messages: Vec<maa::Message>) -> maa::ClientAction {
    maa::ClientAction {
        action: Some(maa::client_action::Action::AddMessagesToTask(
            maa::client_action::AddMessagesToTask {
                task_id: task_id.to_string(),
                messages,
            },
        )),
    }
}

fn add_messages_event(task_id: &str, messages: Vec<maa::Message>) -> maa::ResponseEvent {
    client_actions_event(vec![add_messages_action(task_id, messages)])
}

fn client_actions_event(actions: Vec<maa::ClientAction>) -> maa::ResponseEvent {
    maa::ResponseEvent {
        r#type: Some(maa::response_event::Type::ClientActions(
            maa::response_event::ClientActions { actions },
        )),
    }
}

fn finished_response_events(request: &maa::Request) -> Vec<maa::ResponseEvent> {
    let stream_ids = stream_ids(request);
    vec![init_event(&stream_ids), finished_event()]
}

fn init_event(stream_ids: &StreamIds) -> maa::ResponseEvent {
    maa::ResponseEvent {
        r#type: Some(maa::response_event::Type::Init(
            maa::response_event::StreamInit {
                conversation_id: stream_ids.conversation_id.clone(),
                request_id: stream_ids.request_id.clone(),
                run_id: stream_ids.run_id.clone(),
            },
        )),
    }
}

fn finished_event() -> maa::ResponseEvent {
    maa::ResponseEvent {
        r#type: Some(maa::response_event::Type::Finished(
            maa::response_event::StreamFinished {
                token_usage: Vec::new(),
                should_refresh_model_config: false,
                request_cost: None,
                conversation_usage_metadata: None,
                reason: Some(maa::response_event::stream_finished::Reason::Done(
                    maa::response_event::stream_finished::Done {},
                )),
            },
        )),
    }
}

fn response_event_stream(events: Vec<maa::ResponseEvent>) -> Response {
    let mut body = String::new();
    for event in events {
        body.push_str(&response_event_sse_chunk(event));
    }

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

fn response_event_receiver_stream(mut rx: mpsc::UnboundedReceiver<maa::ResponseEvent>) -> Response {
    let stream = async_stream::stream! {
        while let Some(event) = rx.recv().await {
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(response_event_sse_chunk(event)));
        }
    };

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

fn response_event_sse_chunk(event: maa::ResponseEvent) -> String {
    let encoded = BASE64_URL_SAFE.encode(event.encode_to_vec());
    format!("data: \"{encoded}\"\n\n")
}

fn send_response_event(tx: &mpsc::UnboundedSender<maa::ResponseEvent>, event: maa::ResponseEvent) {
    let _ = tx.send(event);
}

#[derive(Clone)]
struct StreamIds {
    conversation_id: String,
    request_id: String,
    run_id: String,
}

fn stream_ids(request: &maa::Request) -> StreamIds {
    let conversation_id = request
        .metadata
        .as_ref()
        .map(|metadata| metadata.conversation_id.trim())
        .filter(|conversation_id| !conversation_id.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("local-conversation-{}", Uuid::new_v4()));
    StreamIds {
        conversation_id,
        request_id: format!("local-request-{}", Uuid::new_v4()),
        run_id: Uuid::new_v4().to_string(),
    }
}

struct TaskInfo {
    id: String,
    description: String,
    needs_create: bool,
}

fn task_info(request: &maa::Request) -> TaskInfo {
    if let Some(task) = request
        .task_context
        .as_ref()
        .and_then(|context| context.tasks.first())
    {
        return TaskInfo {
            id: task.id.clone(),
            description: task.description.clone(),
            needs_create: false,
        };
    }

    TaskInfo {
        id: format!("local-task-{}", Uuid::new_v4()),
        description: extract_user_prompt(request)
            .map(|prompt| prompt.lines().next().unwrap_or_default().to_string())
            .filter(|description| !description.trim().is_empty())
            .unwrap_or_else(|| "Local sidecar task".to_string()),
        needs_create: true,
    }
}

#[allow(deprecated)]
fn extract_user_prompt(request: &maa::Request) -> Option<String> {
    use maa::request::input::user_inputs::user_input::Input as UserInput;
    use maa::request::input::Type;

    let input = request.input.as_ref()?;
    match input.r#type.as_ref()? {
        Type::UserInputs(inputs) => {
            inputs
                .inputs
                .iter()
                .find_map(|input| match input.input.as_ref()? {
                    UserInput::UserQuery(query) => Some(query.query.clone()),
                    UserInput::CliAgentUserQuery(query) => {
                        query.user_query.as_ref().map(|query| query.query.clone())
                    }
                    _ => None,
                })
        }
        Type::QueryWithCannedResponse(query) => Some(query.query.clone()),
        Type::AutoCodeDiffQuery(query) => Some(query.query.clone()),
        Type::CreateNewProject(query) => Some(query.query.clone()),
        Type::CloneRepository(query) => Some(format!("Clone repository {}", query.url)),
        Type::SummarizeConversation(query) => Some(query.prompt.clone()),
        Type::CreateEnvironment(query) => Some(format!(
            "Create a development environment for {}",
            query.repo_paths.join(", ")
        )),
        Type::StartFromAmbientRunPrompt(query) => Some(query.runtime_base_prompt.clone()),
        Type::InvokeSkill(query) => query
            .user_query
            .as_ref()
            .map(|query| query.query.clone())
            .or_else(|| Some("Invoke the selected skill.".to_string())),
        Type::UserQuery(query) => Some(query.query.clone()),
        _ => None,
    }
}

fn now_timestamp() -> prost_types::Timestamp {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: now.as_secs() as i64,
        nanos: now.subsec_nanos() as i32,
    }
}

fn required_str<'a>(value: &'a str, field: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("{field} is required");
    }
    Ok(value)
}

fn local_agent_error_message(err: anyhow::Error) -> String {
    format!("Local sidecar LLM request failed:\n\n{err:#}")
}

fn local_agent_error_run(err: anyhow::Error) -> LocalAgentRun {
    LocalAgentRun::from_output(local_agent_error_message(err))
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
    let local = match LocalLlmConfig::load() {
        Ok(config) => match config.models() {
            Ok(models) if !models.is_empty() => {
                available_llms(&models, Some(config.active_model.as_str()))
            }
            Ok(_) => unavailable_llms("No models are configured in llm.toml"),
            Err(err) => {
                log::warn!("Ignoring local LLM config: {err:#}");
                unavailable_llms("The local LLM config is invalid")
            }
        },
        Err(err) => {
            log::warn!("Local LLM config is unavailable: {err:#}");
            unavailable_llms("Create ~/.warp-oss/llm.toml to enable local models")
        }
    };
    json!({
        "agentMode": local.clone(),
        "planning": local.clone(),
        "coding": local.clone(),
        "cliAgent": local.clone(),
        "computerUseAgent": local,
    })
}

fn available_llms(models: &[ResolvedLocalLlm], active_model: Option<&str>) -> Value {
    let default_id = active_model
        .and_then(|active_model| {
            models
                .iter()
                .find(|model| model.id == active_model || model.base_model_name == active_model)
                .map(|model| model.id.as_str())
        })
        .or_else(|| models.first().map(|model| model.id.as_str()))
        .unwrap_or("local");
    json!({
        "defaultId": default_id,
        "preferredCodexModelId": null,
        "choices": models.iter().map(llm_info).collect::<Vec<_>>(),
    })
}

fn unavailable_llms(disable_reason: &str) -> Value {
    json!({
        "defaultId": "local-config-required",
        "preferredCodexModelId": null,
        "choices": [{
            "displayName": "Local Config Required",
            "baseModelName": "local-config-required",
            "id": "local-config-required",
            "reasoningLevel": null,
            "usageMetadata": {
                "creditMultiplier": null,
                "requestMultiplier": 0,
            },
            "description": "Local sidecar model configuration",
            "disableReason": disable_reason,
            "visionSupported": false,
            "spec": null,
            "provider": "UNKNOWN",
            "hostConfigs": [{
                "enabled": false,
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
        }],
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
        "disableReason": if model.token_configured {
            Value::Null
        } else {
            json!("Configure an API token in llm.toml")
        },
        "visionSupported": false,
        "spec": null,
        "provider": model.provider(),
        "hostConfigs": [{
            "enabled": model.token_configured,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_llm_config_resolves_configured_headers() {
        let config: LocalLlmConfig = toml::from_str(
            r#"
active_model = "qwen"

[[providers]]
name = "dashscope"
api_base = "https://example.test/v1"
api_key = "test-token"
api_style = "openai"
description = "DashScope local"

[providers.headers]
X-Test-Header = " enabled "
User-Agent = "OpenAI/Go 3.22.0"

[[models]]
name = "qwen"
provider = "dashscope"
display_name = "Qwen"
description = "Qwen coding"
"#,
        )
        .unwrap();

        let model = config.active_model().unwrap();

        assert_eq!(model.display_name, "Qwen");
        assert_eq!(model.description(), "Qwen coding");
        assert!(model
            .headers
            .contains(&("X-Test-Header".to_string(), "enabled".to_string())));
        assert!(model
            .headers
            .contains(&("User-Agent".to_string(), "OpenAI/Go 3.22.0".to_string())));
    }

    #[test]
    fn parses_openai_tool_calls_from_chat_completion() {
        let response = json!({
            "choices": [{
                "message": {
                    "content": "I'll inspect that file.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }]
                }
            }]
        });

        let turn = parse_openai_assistant_turn(&response).unwrap();

        assert_eq!(turn.content, "I'll inspect that file.");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].name, "read_file");
        assert_eq!(turn.tool_calls[0].arguments["path"], "Cargo.toml");
    }

    #[test]
    fn local_agent_system_prompt_describes_available_tools() {
        let prompt = local_agent_system_prompt();

        assert!(prompt.contains("read_file"));
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("grep"));
        assert!(prompt.contains("bash"));
    }

    #[test]
    fn openai_messages_include_prior_task_messages() {
        let request = maa::Request {
            task_context: Some(maa::request::TaskContext {
                tasks: vec![maa::Task {
                    id: "task-1".to_string(),
                    description: "task".to_string(),
                    dependencies: None,
                    messages: vec![
                        maa::Message {
                            id: "m1".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r1".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::UserQuery(
                                maa::message::UserQuery {
                                    query: "first question".to_string(),
                                    context: None,
                                    referenced_attachments: HashMap::new(),
                                    mode: None,
                                    intended_agent: Default::default(),
                                },
                            )),
                        },
                        maa::Message {
                            id: "m2".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r1".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::AgentOutput(
                                maa::message::AgentOutput {
                                    text: "first answer".to_string(),
                                },
                            )),
                        },
                    ],
                    summary: String::new(),
                    server_data: String::new(),
                }],
            }),
            input: Some(maa::request::Input {
                context: None,
                r#type: Some(maa::request::input::Type::UserInputs(
                    maa::request::input::UserInputs {
                        inputs: vec![maa::request::input::user_inputs::UserInput {
                            input: Some(
                                maa::request::input::user_inputs::user_input::Input::UserQuery(
                                    maa::request::input::UserQuery {
                                        query: "follow up".to_string(),
                                        referenced_attachments: HashMap::new(),
                                        mode: None,
                                        intended_agent: Default::default(),
                                    },
                                ),
                            ),
                        }],
                    },
                )),
            }),
            ..Default::default()
        };

        let messages = openai_messages_for_request(&request);

        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "first question");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "first answer");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "follow up");
    }

    #[test]
    fn local_tool_call_from_api_reconstructs_write_file() {
        let tool_call = maa::message::ToolCall {
            tool_call_id: "call_write".to_string(),
            tool: Some(maa::message::tool_call::Tool::ApplyFileDiffs(
                maa::message::tool_call::ApplyFileDiffs {
                    summary: "Write notes.md".to_string(),
                    diffs: Vec::new(),
                    new_files: vec![maa::message::tool_call::apply_file_diffs::NewFile {
                        file_path: "notes.md".to_string(),
                        content: "local notes".to_string(),
                    }],
                    deleted_files: Vec::new(),
                    v4a_updates: Vec::new(),
                },
            )),
        };

        let local = local_tool_call_from_api(&tool_call).unwrap();

        assert_eq!(local.id, "call_write");
        assert_eq!(local.name, "write_file");
        assert_eq!(local.arguments["path"], "notes.md");
        assert_eq!(local.arguments["content"], "local notes");
        assert_eq!(local.arguments["overwrite"], true);
    }

    #[allow(deprecated)]
    #[test]
    fn openai_messages_include_prior_write_file_tool_history() {
        let request = maa::Request {
            task_context: Some(maa::request::TaskContext {
                tasks: vec![maa::Task {
                    id: "task-1".to_string(),
                    description: "task".to_string(),
                    dependencies: None,
                    messages: vec![
                        maa::Message {
                            id: "m1".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r1".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::ToolCall(
                                maa::message::ToolCall {
                                    tool_call_id: "call_write".to_string(),
                                    tool: Some(maa::message::tool_call::Tool::ApplyFileDiffs(
                                        maa::message::tool_call::ApplyFileDiffs {
                                            summary: "Write notes.md".to_string(),
                                            diffs: Vec::new(),
                                            new_files: vec![
                                                maa::message::tool_call::apply_file_diffs::NewFile {
                                                    file_path: "notes.md".to_string(),
                                                    content: "local notes".to_string(),
                                                },
                                            ],
                                            deleted_files: Vec::new(),
                                            v4a_updates: Vec::new(),
                                        },
                                    )),
                                },
                            )),
                        },
                        maa::Message {
                            id: "m2".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r1".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::ToolCallResult(
                                maa::message::ToolCallResult {
                                    tool_call_id: "call_write".to_string(),
                                    context: None,
                                    result: Some(
                                        maa::message::tool_call_result::Result::ApplyFileDiffs(
                                            maa::ApplyFileDiffsResult {
                                                result: Some(
                                                    maa::apply_file_diffs_result::Result::Success(
                                                        maa::apply_file_diffs_result::Success {
                                                            updated_files: Vec::new(),
                                                            updated_files_v2: vec![
                                                                maa::apply_file_diffs_result::success::UpdatedFileContent {
                                                                    file: Some(maa::FileContent {
                                                                        file_path: "notes.md".to_string(),
                                                                        content: "local notes".to_string(),
                                                                        line_range: None,
                                                                    }),
                                                                    was_edited_by_user: false,
                                                                },
                                                            ],
                                                            deleted_files: Vec::new(),
                                                        },
                                                    ),
                                                ),
                                            },
                                        ),
                                    ),
                                },
                            )),
                        },
                    ],
                    summary: String::new(),
                    server_data: String::new(),
                }],
            }),
            input: Some(maa::request::Input {
                context: None,
                r#type: Some(maa::request::input::Type::UserInputs(
                    maa::request::input::UserInputs {
                        inputs: vec![maa::request::input::user_inputs::UserInput {
                            input: Some(
                                maa::request::input::user_inputs::user_input::Input::UserQuery(
                                    maa::request::input::UserQuery {
                                        query: "continue".to_string(),
                                        referenced_attachments: HashMap::new(),
                                        mode: None,
                                        intended_agent: Default::default(),
                                    },
                                ),
                            ),
                        }],
                    },
                )),
            }),
            ..Default::default()
        };

        let messages = openai_messages_for_request(&request);

        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "write_file"
        );
        let arguments: Value = serde_json::from_str(
            messages[1]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(arguments["path"], "notes.md");
        assert_eq!(arguments["content"], "local notes");
        assert_eq!(arguments["overwrite"], true);
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_write");
        assert_eq!(messages[2]["content"], "notes.md:\nlocal notes");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "continue");
    }

    #[allow(deprecated)]
    #[test]
    fn api_tool_result_text_strips_local_command_header() {
        let result = maa::message::ToolCallResult {
            tool_call_id: "call_bash".to_string(),
            context: None,
            result: Some(maa::message::tool_call_result::Result::RunShellCommand(
                maa::RunShellCommandResult {
                    command: "printf hi".to_string(),
                    output: String::new(),
                    exit_code: 0,
                    result: Some(maa::run_shell_command_result::Result::CommandFinished(
                        maa::ShellCommandFinished {
                            command_id: "call_bash".to_string(),
                            output: "Command: printf hi\nExit code: 0\n\nStdout:\nhi\n".to_string(),
                            exit_code: 0,
                        },
                    )),
                },
            )),
        };

        let text = api_tool_result_text(&result);

        assert_eq!(text, "Command: printf hi\nExit code: 0\n\nStdout:\nhi");
    }

    #[test]
    fn executes_read_file_tool_relative_to_workspace() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("notes.md");
        fs::write(&path, "local notes").unwrap();
        let tool_call = LocalToolCall {
            id: "call_read".to_string(),
            name: "read_file".to_string(),
            arguments: json!({ "path": "notes.md" }),
        };

        let result = execute_local_tool(&tool_call, tempdir.path()).unwrap();

        assert_eq!(result.tool_call_id, "call_read");
        assert_eq!(result.name, "read_file");
        assert_eq!(result.content, "local notes");
    }

    #[test]
    fn read_file_supports_offset_and_limit() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("notes.md");
        fs::write(&path, "line 1\nline 2\nline 3\nline 4\n").unwrap();
        let tool_call = LocalToolCall {
            id: "call_read".to_string(),
            name: "read_file".to_string(),
            arguments: json!({
                "path": "notes.md",
                "offset": 1,
                "limit": 2
            }),
        };

        let result = execute_local_tool(&tool_call, tempdir.path()).unwrap();

        assert_eq!(result.content, "line 2\nline 3\n");
    }

    #[test]
    fn executes_write_file_tool_relative_to_workspace() {
        let tempdir = tempfile::tempdir().unwrap();
        let tool_call = LocalToolCall {
            id: "call_write".to_string(),
            name: "write_file".to_string(),
            arguments: json!({
                "path": "notes.md",
                "content": "local notes"
            }),
        };

        let result = execute_local_tool(&tool_call, tempdir.path()).unwrap();

        assert_eq!(result.tool_call_id, "call_write");
        assert_eq!(result.name, "write_file");
        assert_eq!(
            fs::read_to_string(tempdir.path().join("notes.md")).unwrap(),
            "local notes"
        );
        assert!(result.content.contains("Wrote 11 bytes"));
    }

    #[test]
    fn write_file_refuses_to_overwrite_without_flag() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "existing").unwrap();
        let tool_call = LocalToolCall {
            id: "call_write".to_string(),
            name: "write_file".to_string(),
            arguments: json!({
                "path": "notes.md",
                "content": "new content"
            }),
        };

        let err = execute_local_tool(&tool_call, tempdir.path()).unwrap_err();

        assert!(format!("{err:#}").contains("overwrite=true"));
        assert_eq!(
            fs::read_to_string(tempdir.path().join("notes.md")).unwrap(),
            "existing"
        );
    }

    #[test]
    fn executes_search_replace_tool_relative_to_workspace() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "alpha\nbeta\n").unwrap();
        let tool_call = LocalToolCall {
            id: "call_replace".to_string(),
            name: "search_replace".to_string(),
            arguments: json!({
                "path": "notes.md",
                "search": "beta",
                "replace": "gamma"
            }),
        };

        let result = execute_local_tool(&tool_call, tempdir.path()).unwrap();

        assert_eq!(result.tool_call_id, "call_replace");
        assert_eq!(result.name, "search_replace");
        assert_eq!(
            fs::read_to_string(tempdir.path().join("notes.md")).unwrap(),
            "alpha\ngamma\n"
        );
        assert!(result.content.contains("Replaced 1 occurrence"));
    }

    #[test]
    fn search_replace_requires_unique_match() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "same\nsame\n").unwrap();
        let tool_call = LocalToolCall {
            id: "call_replace".to_string(),
            name: "search_replace".to_string(),
            arguments: json!({
                "path": "notes.md",
                "search": "same",
                "replace": "changed"
            }),
        };

        let err = execute_local_tool(&tool_call, tempdir.path()).unwrap_err();

        assert!(format!("{err:#}").contains("exactly one match"));
        assert_eq!(
            fs::read_to_string(tempdir.path().join("notes.md")).unwrap(),
            "same\nsame\n"
        );
    }

    #[test]
    fn executes_grep_tool_relative_to_workspace() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::create_dir_all(tempdir.path().join("src")).unwrap();
        fs::write(
            tempdir.path().join("src/lib.rs"),
            "alpha beta\nsecond line\n",
        )
        .unwrap();
        fs::write(tempdir.path().join("README.md"), "alpha docs\n").unwrap();
        let tool_call = LocalToolCall {
            id: "call_grep".to_string(),
            name: "grep".to_string(),
            arguments: json!({
                "pattern": "alpha",
                "path": "src",
                "max_matches": 5
            }),
        };

        let result = execute_local_tool(&tool_call, tempdir.path()).unwrap();

        assert_eq!(result.tool_call_id, "call_grep");
        assert_eq!(result.name, "grep");
        assert!(result.content.contains("src/lib.rs:1:alpha beta"));
        assert!(!result.content.contains("README.md"));
    }

    #[test]
    fn executes_bash_tool_in_workspace() {
        let tempdir = tempfile::tempdir().unwrap();
        let tool_call = LocalToolCall {
            id: "call_bash".to_string(),
            name: "bash".to_string(),
            arguments: json!({
                "command": "printf local > out.txt && pwd",
                "timeout_secs": 5
            }),
        };

        let result = execute_local_tool(&tool_call, tempdir.path()).unwrap();

        assert_eq!(result.tool_call_id, "call_bash");
        assert_eq!(result.name, "bash");
        assert_eq!(
            fs::read_to_string(tempdir.path().join("out.txt")).unwrap(),
            "local"
        );
        assert!(result.content.contains("Exit code: 0"));
        assert!(result
            .content
            .contains(&tempdir.path().display().to_string()));
    }

    #[tokio::test]
    async fn agent_loop_sends_tool_result_back_to_model() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "local notes").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = run_openai_agent_loop(
            openai_messages_from_prompt("What is in notes.md?"),
            tempdir.path(),
            {
                let calls = calls.clone();
                move |messages: Vec<Value>| {
                    let calls = calls.clone();
                    async move {
                        match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                            0 => Ok(json!({
                                "choices": [{
                                    "message": {
                                        "content": "",
                                        "tool_calls": [{
                                            "id": "call_read",
                                            "type": "function",
                                            "function": {
                                                "name": "read_file",
                                                "arguments": "{\"path\":\"notes.md\"}"
                                            }
                                        }]
                                    }
                                }]
                            })),
                            1 => {
                                assert!(messages.iter().any(|message| {
                                    message["role"] == "tool"
                                        && message["tool_call_id"] == "call_read"
                                        && message["content"] == "local notes"
                                }));
                                Ok(json!({
                                    "choices": [{
                                        "message": {
                                            "content": "notes.md says: local notes"
                                        }
                                    }]
                                }))
                            }
                            _ => panic!("agent loop called the model too many times"),
                        }
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(output.output, "notes.md says: local notes");
        assert_eq!(output.tool_events.len(), 1);
        assert_eq!(output.tool_events[0].tool_call.name, "read_file");
        assert_eq!(output.tool_events[0].result.content, "local notes");
    }

    #[tokio::test]
    async fn agent_loop_reports_tool_call_before_tool_result() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "local notes").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let progress = Arc::new(std::sync::Mutex::new(Vec::new()));

        let output = run_openai_agent_loop_with_progress(
            openai_messages_from_prompt("What is in notes.md?"),
            tempdir.path(),
            {
                let calls = calls.clone();
                move |_messages: Vec<Value>| {
                    let calls = calls.clone();
                    async move {
                        match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                            0 => Ok(json!({
                                "choices": [{
                                    "message": {
                                        "content": "",
                                        "tool_calls": [{
                                            "id": "call_read",
                                            "type": "function",
                                            "function": {
                                                "name": "read_file",
                                                "arguments": "{\"path\":\"notes.md\"}"
                                            }
                                        }]
                                    }
                                }]
                            })),
                            1 => Ok(json!({
                                "choices": [{
                                    "message": {
                                        "content": "done"
                                    }
                                }]
                            })),
                            _ => panic!("agent loop called the model too many times"),
                        }
                    }
                }
            },
            {
                let progress = progress.clone();
                move |tool_call| {
                    progress
                        .lock()
                        .unwrap()
                        .push(format!("call:{}", tool_call.name));
                }
            },
            {
                let progress = progress.clone();
                move |event| {
                    progress
                        .lock()
                        .unwrap()
                        .push(format!("result:{}", event.result.name));
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(output.output, "done");
        assert_eq!(
            progress.lock().unwrap().as_slice(),
            ["call:read_file", "result:read_file"]
        );
    }

    #[test]
    fn openai_payload_advertises_read_file_tool() {
        let payload = openai_chat_completion_payload(
            "qwen3.6-plus",
            vec![json!({
                "role": "user",
                "content": "read notes.md"
            })],
        );

        assert_eq!(payload["model"], "qwen3.6-plus");
        let tools = payload["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| {
            tool["type"] == "function"
                && tool["function"]["name"] == "read_file"
                && tool["function"]["parameters"]["properties"]["path"]["type"] == "string"
                && tool["function"]["parameters"]["properties"]["offset"]["type"] == "integer"
                && tool["function"]["parameters"]["properties"]["limit"]["type"] == "integer"
        }));
        assert!(tools.iter().any(|tool| {
            tool["type"] == "function"
                && tool["function"]["name"] == "write_file"
                && tool["function"]["parameters"]["properties"]["content"]["type"] == "string"
        }));
        assert!(tools.iter().any(|tool| {
            tool["type"] == "function"
                && tool["function"]["name"] == "search_replace"
                && tool["function"]["parameters"]["properties"]["search"]["type"] == "string"
                && tool["function"]["parameters"]["properties"]["replace"]["type"] == "string"
        }));
        assert!(tools.iter().any(|tool| {
            tool["type"] == "function"
                && tool["function"]["name"] == "grep"
                && tool["function"]["parameters"]["properties"]["pattern"]["type"] == "string"
        }));
        assert!(tools.iter().any(|tool| {
            tool["type"] == "function"
                && tool["function"]["name"] == "bash"
                && tool["function"]["parameters"]["properties"]["command"]["type"] == "string"
        }));
    }

    #[test]
    fn extracts_workspace_from_request_context_directory() {
        let request = maa::Request {
            input: Some(maa::request::Input {
                context: Some(maa::InputContext {
                    directory: Some(maa::input_context::Directory {
                        pwd: "/tmp/warp-workspace".to_string(),
                        home: "/tmp".to_string(),
                        pwd_file_symbols_indexed: false,
                    }),
                    ..Default::default()
                }),
                r#type: None,
            }),
            ..Default::default()
        };

        assert_eq!(
            workspace_for_request(&request),
            PathBuf::from("/tmp/warp-workspace")
        );
    }

    #[tokio::test]
    async fn openai_compatible_call_runs_tools_against_workspace() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "local notes").unwrap();

        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let headers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/chat/completions",
            post({
                let bodies = bodies.clone();
                let headers = headers.clone();
                let calls = calls.clone();
                move |request_headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let bodies = bodies.clone();
                    let headers = headers.clone();
                    let calls = calls.clone();
                    async move {
                        headers.lock().unwrap().push(request_headers);
                        bodies.lock().unwrap().push(body);
                        match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                            0 => Json(json!({
                                "choices": [{
                                    "message": {
                                        "content": "",
                                        "tool_calls": [{
                                            "id": "call_read",
                                            "type": "function",
                                            "function": {
                                                "name": "read_file",
                                                "arguments": "{\"path\":\"notes.md\"}"
                                            }
                                        }]
                                    }
                                }]
                            })),
                            _ => Json(json!({
                                "choices": [{
                                    "message": {
                                        "content": "notes.md says: local notes"
                                    }
                                }]
                            })),
                        }
                    }
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let model = ResolvedLocalLlm {
            id: "local".to_string(),
            display_name: "Local".to_string(),
            base_model_name: "qwen3.6-plus".to_string(),
            base_url: format!("http://{addr}"),
            api_style: "openai".to_string(),
            token: "test-token".to_string(),
            token_configured: true,
            headers: vec![("User-Agent".to_string(), "OpenAI/Go 3.22.0".to_string())],
            description: None,
        };

        let output = call_openai_compatible(
            &reqwest::Client::new(),
            &model,
            openai_messages_from_prompt("What is in notes.md?"),
            tempdir.path(),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(output.output, "notes.md says: local notes");
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        let headers = headers.lock().unwrap();
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().all(|headers| {
            headers
                .get(header::USER_AGENT)
                .is_some_and(|value| value == "OpenAI/Go 3.22.0")
        }));
        assert!(bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| { tool["type"] == "function" && tool["function"]["name"] == "read_file" }));
        assert!(bodies[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "tool"
                    && message["tool_call_id"] == "call_read"
                    && message["content"] == "local notes"
            }));
    }

    #[test]
    fn agent_response_events_include_tool_messages() {
        let request = maa::Request::default();
        let run = LocalAgentRun {
            output: "done".to_string(),
            tool_events: vec![LocalToolEvent {
                tool_call: LocalToolCall {
                    id: "call_read".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({ "path": "notes.md" }),
                },
                result: LocalToolResult {
                    tool_call_id: "call_read".to_string(),
                    name: "read_file".to_string(),
                    content: "local notes".to_string(),
                },
            }],
        };

        let events = agent_response_events(&request, run);
        let messages = events
            .iter()
            .filter_map(|event| match event.r#type.as_ref()? {
                maa::response_event::Type::ClientActions(actions) => Some(actions),
                _ => None,
            })
            .flat_map(|actions| actions.actions.iter())
            .filter_map(|action| match action.action.as_ref()? {
                maa::client_action::Action::AddMessagesToTask(add) => Some(add.messages.as_slice()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();

        assert!(matches!(
            messages[0].message,
            Some(maa::message::Message::ToolCall(_))
        ));
        assert!(matches!(
            messages[1].message,
            Some(maa::message::Message::ToolCallResult(_))
        ));
        assert!(matches!(
            messages[2].message,
            Some(maa::message::Message::AgentOutput(_))
        ));
    }
}
