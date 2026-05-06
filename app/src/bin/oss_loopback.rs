use std::{
    collections::HashMap,
    fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{prelude::BASE64_URL_SAFE, Engine as _};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use uuid::Uuid;
use warp_multi_agent_api as maa;

const LOCAL_ACCOUNT_FILE: &str = "local-account.json";
const LOCAL_LLM_FILE: &str = "llm.toml";
const LOCAL_AGENT_MAX_TURNS: usize = 8;
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
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmConfigModel {
    name: String,
    provider: String,
    alias: Option<String>,
    id: Option<String>,
    display_name: Option<String>,
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

    let output = match generate_local_agent_output(&state, &request).await {
        Ok(output) => output,
        Err(err) => local_agent_error_message(err),
    };

    response_event_stream(agent_response_events(&request, output))
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

async fn generate_local_agent_output(
    state: &ServerState,
    request: &maa::Request,
) -> Result<String> {
    let prompt = extract_user_prompt(request)
        .filter(|prompt| !prompt.trim().is_empty())
        .unwrap_or_else(|| "Continue the current Warp agent conversation.".to_string());

    let model = LocalLlmConfig::load()?.active_model()?;

    match model.api_style.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => call_anthropic_compatible(&state.client, &model, &prompt).await,
        "openai" | "openai-compatible" | "openai_compatible" | "xai" | "grok" | "google"
        | "gemini" | "openrouter" => call_openai_compatible(&state.client, &model, &prompt).await,
        _ => call_openai_compatible(&state.client, &model, &prompt).await,
    }
}

async fn call_openai_compatible(
    client: &reqwest::Client,
    model: &ResolvedLocalLlm,
    prompt: &str,
) -> Result<String> {
    let url = completion_url(&model.base_url, "chat/completions");
    let mut request = client.post(url).json(&json!({
        "model": model.base_model_name,
        "messages": [
            {
                "role": "system",
                "content": "You are running inside a local Warp OSS sidecar. Answer directly. Do not claim to have tool access."
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "stream": false,
    }));
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

    let value: Value = serde_json::from_str(&body).context("failed to parse local LLM response")?;
    extract_openai_text(&value).context("local LLM response did not contain message content")
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
    vec![json!({
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
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }
    })]
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

fn extract_openai_text(value: &Value) -> Option<String> {
    value
        .pointer("/choices/0/message/content")
        .and_then(text_value)
        .or_else(|| value.pointer("/choices/0/text").and_then(text_value))
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

fn execute_local_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    match tool_call.name.as_str() {
        "read_file" => {
            let path = tool_call
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .context("read_file requires a non-empty path")?;
            let resolved = resolve_workspace_path(workspace, path)?;
            let content = fs::read_to_string(&resolved)
                .with_context(|| format!("failed to read {}", resolved.display()))?;
            Ok(LocalToolResult {
                tool_call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                content,
            })
        }
        name => anyhow::bail!("unsupported local tool: {name}"),
    }
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

async fn run_openai_agent_loop<F, Fut>(
    prompt: &str,
    workspace: &Path,
    mut complete: F,
) -> Result<String>
where
    F: FnMut(Vec<Value>) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let mut messages = vec![
        json!({
            "role": "system",
            "content": "You are running inside a local Warp OSS sidecar. Answer directly and use local tools when they are useful."
        }),
        json!({
            "role": "user",
            "content": prompt
        }),
    ];

    for _ in 0..LOCAL_AGENT_MAX_TURNS {
        let response = complete(messages.clone()).await?;
        let turn = parse_openai_assistant_turn(&response)?;
        if turn.tool_calls.is_empty() {
            return Ok(turn.content);
        }

        messages.push(openai_assistant_message(&turn));
        for tool_call in &turn.tool_calls {
            let result =
                execute_local_tool(tool_call, workspace).unwrap_or_else(|err| LocalToolResult {
                    tool_call_id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    content: format!("Tool failed: {err:#}"),
                });
            messages.push(openai_tool_result_message(&result));
        }
    }

    anyhow::bail!("local agent exceeded {LOCAL_AGENT_MAX_TURNS} tool turns")
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

fn agent_response_events(request: &maa::Request, output: String) -> Vec<maa::ResponseEvent> {
    let stream_ids = stream_ids(request);
    let task_info = task_info(request);
    let message = maa::Message {
        id: format!("local-message-{}", Uuid::new_v4()),
        task_id: task_info.id.clone(),
        request_id: stream_ids.request_id.clone(),
        timestamp: Some(now_timestamp()),
        server_message_data: String::new(),
        citations: Vec::new(),
        message: Some(maa::message::Message::AgentOutput(
            maa::message::AgentOutput { text: output },
        )),
    };

    let mut actions = Vec::new();
    if task_info.needs_create {
        actions.push(maa::ClientAction {
            action: Some(maa::client_action::Action::CreateTask(
                maa::client_action::CreateTask {
                    task: Some(maa::Task {
                        id: task_info.id.clone(),
                        description: task_info.description,
                        dependencies: None,
                        messages: Vec::new(),
                        summary: String::new(),
                        server_data: String::new(),
                    }),
                },
            )),
        });
    }
    actions.push(maa::ClientAction {
        action: Some(maa::client_action::Action::AddMessagesToTask(
            maa::client_action::AddMessagesToTask {
                task_id: task_info.id,
                messages: vec![message],
            },
        )),
    });

    vec![
        init_event(&stream_ids),
        maa::ResponseEvent {
            r#type: Some(maa::response_event::Type::ClientActions(
                maa::response_event::ClientActions { actions },
            )),
        },
        finished_event(),
    ]
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
        let encoded = BASE64_URL_SAFE.encode(event.encode_to_vec());
        body.push_str("data: \"");
        body.push_str(&encoded);
        body.push_str("\"\n\n");
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn agent_loop_sends_tool_result_back_to_model() {
        let tempdir = tempfile::tempdir().unwrap();
        fs::write(tempdir.path().join("notes.md"), "local notes").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = run_openai_agent_loop("What is in notes.md?", tempdir.path(), {
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
        })
        .await
        .unwrap();

        assert_eq!(output, "notes.md says: local notes");
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
        }));
    }
}
