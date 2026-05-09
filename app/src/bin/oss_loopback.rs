use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use std::{process::Stdio, time::Instant};

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, State,
    },
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{prelude::BASE64_URL_SAFE, Engine as _};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use session_sharing_protocol::{
    common::{
        AbsentViewer, ActivePrompt, CommandExecutionFailureReason, CommandExecutionRequestId,
        ControlActionFailureReason, ControlActionRequestId, InputReplicaId, InputUpdate,
        OrderedTerminalEvent, OrderedTerminalEventType, ParticipantId, ParticipantInfo,
        ParticipantList, ParticipantPresenceUpdate, PresenceUpdate, PresentViewer, ProfileData,
        Role, RoleRequestRejectedReason, RoleRequestResponse, Scrollback, Selection, SessionId,
        SessionSecret, UniversalDeveloperInputContext, Viewer, WindowSize, WriteToPtyFailureReason,
    },
    sharer::{self as sharer_protocol, ReconnectToken},
    viewer::{self as viewer_protocol, RoleUpdatedReason},
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, RwLock},
};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};
use warp_multi_agent_api as maa;

#[path = "oss_loopback/agent_providers.rs"]
mod agent_providers;
#[path = "oss_loopback/agent_state.rs"]
mod agent_state;
#[path = "oss_loopback/cloud_agent.rs"]
mod cloud_agent;
#[path = "oss_loopback/coordinator.rs"]
mod coordinator;
#[path = "oss_loopback/probe_discovery.rs"]
mod probe_discovery;
#[path = "oss_loopback/state.rs"]
mod state;
#[path = "oss_loopback/worker.rs"]
mod worker;
#[path = "oss_loopback/worker_discovery.rs"]
mod worker_discovery;

use agent_state::LocalCommandOutput;
use agent_state::{
    LocalAgentRun, LocalAssistantTurn, LocalToolCall, LocalToolEvent, LocalToolResult,
};
use state::{LocalAccount, LocalAgentWorkerConfig};

const LOCAL_LLM_FILE: &str = "llm.toml";
const LOCAL_TOOL_DEFAULT_COMMAND_TIMEOUT_SECS: usize = 30;
const LOCAL_TOOL_DEFAULT_GREP_MATCHES: usize = 100;
const LOCAL_TOOL_MAX_COMMAND_OUTPUT_BYTES: usize = 64_000;
const LOCAL_TOOL_MAX_COMMAND_TIMEOUT_SECS: usize = 120;
const LOCAL_TOOL_MAX_GREP_BYTES: usize = 64_000;
const LOCAL_TOOL_MAX_GREP_MATCHES: usize = 1_000;
const LOCAL_TOOL_MAX_READ_BYTES: usize = 64_000;
const LOCAL_TOOL_MAX_WRITE_BYTES: usize = 64_000;
const TOKEN_TTL_SECONDS: &str = "3600";

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmConfig {
    active_model: String,
    #[serde(default)]
    agent: LocalLlmAgentConfig,
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
            reasoning_field_name: provider
                .reasoning_field_name
                .as_deref()
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .unwrap_or("reasoning_content")
                .to_owned(),
            thinking: model
                .thinking
                .as_deref()
                .map(str::trim)
                .filter(|thinking| !thinking.is_empty())
                .unwrap_or("auto")
                .to_owned(),
            thinking_budget: model.thinking_budget,
            description: model
                .description
                .as_deref()
                .or(provider.description.as_deref())
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(ToOwned::to_owned),
            system_prompt: self.agent.resolve_system_prompt()?,
            enabled_tools: self.agent.enabled_tools_set(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalLlmAgentConfig {
    system_prompt: Option<String>,
    system_prompt_file: Option<String>,
    #[serde(default)]
    enabled_tools: Vec<String>,
}

impl LocalLlmAgentConfig {
    fn resolve_system_prompt(&self) -> Result<Option<String>> {
        if let Some(path) = self
            .system_prompt_file
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            Ok(Some(resolve_agent_system_prompt(path)?))
        } else {
            Ok(self.system_prompt.clone())
        }
    }

    fn enabled_tools_set(&self) -> Option<HashSet<String>> {
        if self.enabled_tools.is_empty() {
            return None;
        }

        Some(
            self.enabled_tools
                .iter()
                .map(|name| name.trim())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        )
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
    reasoning_field_name: Option<String>,
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
    thinking: Option<String>,
    thinking_budget: Option<u32>,
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
    #[allow(dead_code)]
    reasoning_field_name: String,
    thinking: String,
    thinking_budget: Option<u32>,
    description: Option<String>,
    system_prompt: Option<String>,
    enabled_tools: Option<HashSet<String>>,
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

    fn enabled_tools(&self) -> Option<&HashSet<String>> {
        self.enabled_tools.as_ref()
    }

    fn configured_system_prompt(&self) -> Option<&str> {
        self.system_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
    }
}

#[derive(Clone)]
struct ServerState {
    account: Arc<LocalAccount>,
    client: reqwest::Client,
    cloud_agent_runs: cloud_agent::CloudAgentRunStore,
    discovered_workers: worker_discovery::DiscoveredWorkerStore,
    shared_sessions: SharedSessionStore,
    worker_config: Arc<LocalAgentWorkerConfig>,
    worker_runs: worker::WorkerRunStore,
}

type SharedSessionStore = Arc<RwLock<HashMap<SessionId, SharedSession>>>;

#[derive(Clone)]
struct ViewerState {
    tx: mpsc::UnboundedSender<WsMessage>,
    firebase_uid: String,
    display_name: String,
    selection: Selection,
    role: Role,
}

struct SharedSession {
    reconnect_token: ReconnectToken,
    sharer_id: ParticipantId,
    sharer_firebase_uid: String,
    sharer_display_name: String,
    sharer_selection: Selection,
    scrollback: Scrollback,
    active_prompt: ActivePrompt,
    window_size: WindowSize,
    init_block_id: session_sharing_protocol::common::BlockId,
    input_replica_id: InputReplicaId,
    universal_developer_input_context: Option<UniversalDeveloperInputContext>,
    source_type: sharer_protocol::SessionSourceType,
    events: BTreeMap<usize, OrderedTerminalEvent>,
    sharer_tx: Option<mpsc::UnboundedSender<WsMessage>>,
    viewers: HashMap<ParticipantId, ViewerState>,
    agent_run_id: Option<String>,
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
        let worker_config = Arc::new(match LocalAgentWorkerConfig::load() {
            Ok(config) => config,
            Err(err) => {
                log::warn!("Ignoring local agent worker config: {err:#}");
                LocalAgentWorkerConfig::default()
            }
        });
        let state = ServerState {
            account,
            client,
            cloud_agent_runs: cloud_agent::new_cloud_agent_run_store(),
            discovered_workers: worker_discovery::new_discovered_worker_store(),
            shared_sessions: Arc::new(RwLock::new(HashMap::new())),
            worker_config,
            worker_runs: worker::new_worker_run_store(),
        };
        worker_discovery::seed_static_workers(&state);

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
            .route("/api/v1/agent/run", post(cloud_agent::spawn_agent))
            .route("/api/v1/agent/runs", get(cloud_agent::list_agent_runs))
            .route(
                "/api/v1/agent/runs/{run_id}",
                get(cloud_agent::get_agent_run),
            )
            .route(
                "/api/v1/agent/runs/{run_id}/followups",
                post(cloud_agent::submit_agent_followup),
            )
            .route(
                "/api/v1/agent/tasks/{run_id}/cancel",
                post(cloud_agent::cancel_agent_run),
            )
            .route("/ai/multi-agent", post(multi_agent))
            .route("/ai/passive-suggestions", post(passive_suggestions))
            .route("/proxy/customToken", post(proxy_token))
            .route("/proxy/token", post(proxy_token))
            .route("/session/{session_id}", get(session_page))
            .route("/sessions/create", get(create_session_ws))
            .route("/sessions/join/{session_id}", get(join_session_ws))
            .route("/sessions/{session_id}/resume", get(resume_session_ws))
            .route(
                "/worker/discovered",
                get(worker_discovery::discovered_workers),
            )
            .with_state(state.clone());

        let worker_listener = if state.worker_config.enabled {
            let bind_addr = state.worker_config.bind_addr()?;
            let listener = std::net::TcpListener::bind(bind_addr)
                .context("failed to bind WarpSOLO agent worker listener")?;
            listener
                .set_nonblocking(true)
                .context("failed to configure WarpSOLO agent worker listener")?;
            Some(listener)
        } else {
            None
        };
        let worker_addr = worker_listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
            .map(|addr| addr);
        let worker_root_url = worker_addr.map(|addr| format!("http://{addr}"));

        probe_discovery::start(state.clone(), &runtime);

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

        if let Some(std_listener) = worker_listener {
            let router = Router::new()
                .route("/worker/health", get(worker_health))
                .route("/worker/capabilities", get(worker_capabilities))
                .route("/worker/runs", post(worker::create_worker_run))
                .route("/worker/runs/{run_id}", get(worker::get_worker_run))
                .route(
                    "/worker/runs/{run_id}/events",
                    get(worker::worker_run_events),
                )
                .route(
                    "/worker/runs/{run_id}/followup",
                    post(worker::followup_worker_run),
                )
                .route(
                    "/worker/runs/{run_id}/cancel",
                    post(worker::cancel_worker_run),
                )
                .with_state(state);
            runtime.spawn(async move {
                let listener = match tokio::net::TcpListener::from_std(std_listener) {
                    Ok(listener) => listener,
                    Err(err) => {
                        log::warn!("Failed to adopt WarpSOLO agent worker listener: {err:#}");
                        return;
                    }
                };
                if let Err(err) = axum::serve(listener, router).await {
                    log::warn!("WarpSOLO agent worker server exited: {err:#}");
                }
            });
        }

        let server_root_url = format!("http://{addr}");
        log::info!("Started OSS loopback server at {server_root_url}");
        if let Some(worker_root_url) = &worker_root_url {
            log::info!("Started WarpSOLO agent worker at {worker_root_url}");
        }

        Ok(Self {
            _runtime: runtime,
            server_root_url,
        })
    }

    pub fn server_root_url(&self) -> &str {
        &self.server_root_url
    }

    pub fn session_sharing_server_url(&self) -> String {
        self.server_root_url.replacen("http://", "ws://", 1)
    }
}

async fn healthz(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "userId": state.account.user_id,
        "deviceId": state.account.device_id,
    }))
}

async fn worker_health(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "app": "WarpSOLO",
        "version": 1,
        "deviceId": state.account.device_id,
        "userId": state.account.user_id,
        "displayName": state.account.display_name,
    }))
}

async fn worker_capabilities(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({
        "version": 1,
        "deviceId": state.account.device_id,
        "displayName": state.account.display_name,
        "capabilities": ["agent", "terminal", "workspace"],
        "auth": worker_discovery::worker_auth(&state.worker_config),
    }))
}

async fn proxy_token(State(state): State<ServerState>) -> Json<Value> {
    Json(firebase_token_response(&state.account))
}

async fn session_page(AxumPath(session_id): AxumPath<String>) -> Response {
    if session_id.parse::<SessionId>().is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let native_url = format!("warposs://shared_session/{session_id}");
    Html(format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta http-equiv="refresh" content="0; url={native_url}">
  <title>Open WarpSOLO Session</title>
</head>
<body>
  <a href="{native_url}">Open WarpSOLO session</a>
  <script>location.href = "{native_url}";</script>
</body>
</html>"#
    ))
    .into_response()
}

async fn create_agent_shared_session(
    state: &ServerState,
    agent_run_id: &str,
    title: &str,
    prompt: &str,
) -> SessionId {
    let session_id = SessionId::new();
    let sharer_id = ParticipantId::new();
    let input_replica_id = InputReplicaId::from(Uuid::new_v4().to_string());
    let init_block_id = session_sharing_protocol::common::BlockId::from(Uuid::new_v4().to_string());
    let events = cloud_agent_initial_events(agent_run_id, title, prompt)
        .into_iter()
        .enumerate()
        .map(|(event_no, event)| (event_no, ordered_agent_response_event(event_no, event)))
        .collect();

    let session = SharedSession {
        reconnect_token: ReconnectToken::new(),
        sharer_id,
        sharer_firebase_uid: state.account.user_id.clone(),
        sharer_display_name: state.account.display_name.clone(),
        sharer_selection: Selection::None,
        scrollback: Scrollback {
            blocks: Vec::new(),
            is_alt_screen_active: false,
        },
        active_prompt: ActivePrompt::PS1,
        window_size: WindowSize {
            num_rows: 40,
            num_cols: 120,
        },
        init_block_id,
        input_replica_id,
        universal_developer_input_context: None,
        source_type: sharer_protocol::SessionSourceType::AmbientAgent {
            task_id: Some(agent_run_id.to_string()),
        },
        events,
        sharer_tx: None,
        viewers: HashMap::new(),
        agent_run_id: Some(agent_run_id.to_string()),
    };

    state
        .shared_sessions
        .write()
        .await
        .insert(session_id.clone(), session);
    session_id
}

async fn append_agent_shared_session_response_event(
    state: &ServerState,
    session_id: SessionId,
    event: maa::ResponseEvent,
) {
    let mut sessions = state.shared_sessions.write().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        return;
    };
    let event_no = session
        .events
        .keys()
        .next_back()
        .map_or(0, |event_no| *event_no + 1);
    let event = ordered_agent_response_event(event_no, event);
    session.events.insert(event_no, event.clone());
    fanout_viewers(
        session,
        viewer_protocol::DownstreamMessage::OrderedTerminalEvent(event),
    );
}

fn ordered_agent_response_event(
    event_no: usize,
    event: maa::ResponseEvent,
) -> OrderedTerminalEvent {
    OrderedTerminalEvent {
        event_no,
        event_type: OrderedTerminalEventType::AgentResponseEvent {
            response_initiator: None,
            response_event: warp::terminal::shared_session::ai_agent::encode_agent_response_event(
                &event,
            ),
            forked_from_conversation_token: None,
        },
    }
}

async fn create_session_ws(State(state): State<ServerState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_create_session_socket(state, socket))
        .into_response()
}

async fn join_session_ws(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_join_session_socket(state, session_id, socket))
        .into_response()
}

async fn resume_session_ws(
    State(state): State<ServerState>,
    AxumPath(session_id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_resume_session_socket(state, session_id, socket))
        .into_response()
}

async fn handle_create_session_socket(state: ServerState, socket: WebSocket) {
    let (out_tx, mut incoming) = split_loopback_socket(socket);
    let Some(Ok(WsMessage::Text(message))) = incoming.next().await else {
        return;
    };

    let init = match sharer_protocol::UpstreamMessage::from_json(&message) {
        Ok(sharer_protocol::UpstreamMessage::Initialize(init)) => init,
        Ok(_) | Err(_) => {
            let _ = send_sharer_message(
                &out_tx,
                sharer_protocol::DownstreamMessage::FailedToInitializeSession {
                    reason: sharer_protocol::FailedToInitializeSessionReason::internal_server_error_without_details(),
                },
            );
            return;
        }
    };

    let session_id = SessionId::new();
    let session_secret = SessionSecret::new();
    let reconnect_token = ReconnectToken::new();
    let sharer_id = ParticipantId::new();
    let sharer_firebase_uid = state.account.user_id.clone();
    let sharer_display_name = state.account.display_name.clone();

    {
        let mut sessions = state.shared_sessions.write().await;
        sessions.insert(
            session_id,
            SharedSession {
                reconnect_token: reconnect_token.clone(),
                sharer_id: sharer_id.clone(),
                sharer_firebase_uid: sharer_firebase_uid.clone(),
                sharer_display_name,
                sharer_selection: init.selection.clone(),
                scrollback: init.scrollback,
                active_prompt: init.active_prompt,
                window_size: init.window_size,
                init_block_id: init.init_block_id,
                input_replica_id: init.input_replica_id,
                universal_developer_input_context: init.universal_developer_input_context,
                source_type: init.source_type,
                events: BTreeMap::new(),
                sharer_tx: Some(out_tx.clone()),
                viewers: HashMap::new(),
                agent_run_id: None,
            },
        );
    }

    let _ = send_sharer_message(
        &out_tx,
        sharer_protocol::DownstreamMessage::SessionInitialized {
            session_id,
            session_secret,
            reconnect_token,
            sharer_id,
            sharer_firebase_uid,
        },
    );

    handle_sharer_messages(state, session_id, out_tx, incoming).await;
}

async fn handle_resume_session_socket(state: ServerState, session_id: String, socket: WebSocket) {
    let Ok(session_id) = session_id.parse::<SessionId>() else {
        return;
    };
    let (out_tx, mut incoming) = split_loopback_socket(socket);
    let Some(Ok(WsMessage::Text(message))) = incoming.next().await else {
        return;
    };

    let reconnect = match sharer_protocol::UpstreamMessage::from_json(&message) {
        Ok(sharer_protocol::UpstreamMessage::Reconnect(reconnect)) => reconnect,
        Ok(_) | Err(_) => {
            let _ = send_sharer_message(
                &out_tx,
                sharer_protocol::DownstreamMessage::FailedToReconnect {
                    reason: sharer_protocol::ReconnectionFailedReason::Invalid,
                },
            );
            return;
        }
    };

    let mut sessions = state.shared_sessions.write().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        let _ = send_sharer_message(
            &out_tx,
            sharer_protocol::DownstreamMessage::FailedToReconnect {
                reason: sharer_protocol::ReconnectionFailedReason::SessionNotFound,
            },
        );
        return;
    };
    if reconnect.reconnect_token != session.reconnect_token {
        let _ = send_sharer_message(
            &out_tx,
            sharer_protocol::DownstreamMessage::FailedToReconnect {
                reason: sharer_protocol::ReconnectionFailedReason::WrongReconnectionToken,
            },
        );
        return;
    }

    session.sharer_tx = Some(out_tx.clone());
    session.sharer_selection = reconnect.selection;
    let last_received_event_no = session.events.keys().next_back().copied();
    let participant_list = participant_list(session);
    drop(sessions);

    let _ = send_sharer_message(
        &out_tx,
        sharer_protocol::DownstreamMessage::SessionReconnected {
            last_received_event_no,
            participant_list,
        },
    );
    handle_sharer_messages(state, session_id, out_tx, incoming).await;
}

async fn handle_join_session_socket(state: ServerState, session_id: String, socket: WebSocket) {
    let Ok(session_id) = session_id.parse::<SessionId>() else {
        return;
    };
    let (out_tx, mut incoming) = split_loopback_socket(socket);
    let Some(Ok(WsMessage::Text(message))) = incoming.next().await else {
        return;
    };

    let init = match viewer_protocol::UpstreamMessage::from_json(&message) {
        Ok(viewer_protocol::UpstreamMessage::Initialize(init)) => init,
        Ok(_) | Err(_) => {
            let _ = send_viewer_message(
                &out_tx,
                viewer_protocol::DownstreamMessage::FailedToJoin {
                    reason: viewer_protocol::FailedToJoinReason::Invalid,
                },
            );
            return;
        }
    };

    let mut sessions = state.shared_sessions.write().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        let _ = send_viewer_message(
            &out_tx,
            viewer_protocol::DownstreamMessage::FailedToJoin {
                reason: viewer_protocol::FailedToJoinReason::SessionNotFound,
            },
        );
        return;
    };

    let viewer_id = init.viewer_id.unwrap_or_else(ParticipantId::new);
    let rejoining = session.viewers.contains_key(&viewer_id);
    let viewer_firebase_uid = state.account.user_id.clone();
    session.viewers.insert(
        viewer_id.clone(),
        ViewerState {
            tx: out_tx.clone(),
            firebase_uid: viewer_firebase_uid.clone(),
            display_name: state.account.display_name.clone(),
            selection: Selection::None,
            role: Role::Reader,
        },
    );
    let participant_list = participant_list(session);
    let replay_events = events_after(session, init.last_received_event_no);
    let agent_run_id = session.agent_run_id.clone();

    if rejoining {
        let _ = send_viewer_message(
            &out_tx,
            viewer_protocol::DownstreamMessage::RejoinedSuccessfully {
                participant_list: Box::new(participant_list.clone()),
            },
        );
    } else {
        #[allow(deprecated)]
        let joined = viewer_protocol::DownstreamMessage::JoinedSuccessfully {
            scrollback: Box::new(session.scrollback.clone()),
            active_prompt: session.active_prompt.clone(),
            latest_event_no: session.events.keys().next_back().copied(),
            window_size: session.window_size,
            participant_list: Box::new(participant_list.clone()),
            viewer_id: viewer_id.clone(),
            viewer_firebase_uid,
            init_block_id: session.init_block_id.clone(),
            input_replica_id: session.input_replica_id.clone(),
            universal_developer_input_context: session.universal_developer_input_context.clone(),
            source_type: (&session.source_type).into(),
            detailed_source_type: session.source_type.clone(),
        };
        let _ = send_viewer_message(&out_tx, joined);
    }

    fanout_participant_list(session, participant_list);
    drop(sessions);
    if let Some(agent_run_id) = agent_run_id {
        cloud_agent::mark_session_joined(&state.cloud_agent_runs, &agent_run_id).await;
    }

    for event in replay_events {
        let _ = send_viewer_message(
            &out_tx,
            viewer_protocol::DownstreamMessage::OrderedTerminalEvent(event),
        );
    }
    handle_viewer_messages(state, session_id, viewer_id, out_tx, incoming).await;
}

fn split_loopback_socket(
    socket: WebSocket,
) -> (
    mpsc::UnboundedSender<WsMessage>,
    futures_util::stream::SplitStream<WebSocket>,
) {
    let (mut outgoing, incoming) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
    tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if outgoing.send(message).await.is_err() {
                break;
            }
        }
    });
    (out_tx, incoming)
}

async fn handle_sharer_messages(
    state: ServerState,
    session_id: SessionId,
    out_tx: mpsc::UnboundedSender<WsMessage>,
    mut incoming: futures_util::stream::SplitStream<WebSocket>,
) {
    while let Some(Ok(WsMessage::Text(message))) = incoming.next().await {
        let Ok(message) = sharer_protocol::UpstreamMessage::from_json(&message) else {
            continue;
        };

        match message {
            sharer_protocol::UpstreamMessage::Ping { data } => {
                let _ =
                    send_sharer_message(&out_tx, sharer_protocol::DownstreamMessage::Pong { data });
            }
            sharer_protocol::UpstreamMessage::EndSession { reason } => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.remove(&session_id) else {
                    break;
                };
                let viewer_reason = match reason {
                    sharer_protocol::SessionEndedReason::EndedBySharer => {
                        viewer_protocol::SessionEndedReason::EndedBySharer
                    }
                    sharer_protocol::SessionEndedReason::InactivityLimitReached => {
                        viewer_protocol::SessionEndedReason::InactivityLimitReached
                    }
                    sharer_protocol::SessionEndedReason::ExceededSizeLimit => {
                        viewer_protocol::SessionEndedReason::ExceededSizeLimit
                    }
                };
                for viewer in session.viewers.values() {
                    let _ = send_viewer_message(
                        &viewer.tx,
                        viewer_protocol::DownstreamMessage::SessionEnded {
                            reason: viewer_reason,
                        },
                    );
                }
                break;
            }
            sharer_protocol::UpstreamMessage::OrderedTerminalEvent(event) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                let event_no = event.event_no;
                session.events.insert(event_no, event.clone());
                for viewer in session.viewers.values() {
                    let _ = send_viewer_message(
                        &viewer.tx,
                        viewer_protocol::DownstreamMessage::OrderedTerminalEvent(event.clone()),
                    );
                }
                let _ = send_sharer_message(
                    &out_tx,
                    sharer_protocol::DownstreamMessage::EventsProcessedAck {
                        latest_processed_event_no: event_no,
                    },
                );
            }
            sharer_protocol::UpstreamMessage::UpdateSelection(update) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                session.sharer_selection = update.selection.clone();
                let presence = ParticipantPresenceUpdate {
                    participant_id: session.sharer_id.clone(),
                    update: PresenceUpdate::Selection(update.selection),
                };
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::ParticipantPresenceUpdated(presence),
                );
            }
            sharer_protocol::UpstreamMessage::UpdateActivePrompt(update) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                session.active_prompt = update.active_prompt.clone();
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::ActivePromptUpdated(update),
                );
            }
            sharer_protocol::UpstreamMessage::UpdateUniversalDeveloperInputContext(update) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                let current = session
                    .universal_developer_input_context
                    .take()
                    .unwrap_or_default();
                session.universal_developer_input_context =
                    Some(update.clone().merge_into(current));
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::UniversalDeveloperInputContextUpdated(
                        update,
                    ),
                );
            }
            sharer_protocol::UpstreamMessage::UpdateInput(update) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::InputUpdated(update),
                );
            }
            sharer_protocol::UpstreamMessage::UpdateRole {
                participant_id,
                role,
            } => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                if let Some(viewer) = session.viewers.get_mut(&participant_id) {
                    viewer.role = role;
                }
                let participant_list = participant_list(session);
                fanout_participant_list(session, participant_list);
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::ParticipantRoleChanged {
                        participant_id,
                        reason: RoleUpdatedReason::UpdatedBySharer,
                        role,
                    },
                );
            }
            _ => {}
        }
    }

    let mut sessions = state.shared_sessions.write().await;
    if let Some(session) = sessions.get_mut(&session_id) {
        if session
            .sharer_tx
            .as_ref()
            .is_some_and(|tx| tx.same_channel(&out_tx))
        {
            session.sharer_tx = None;
        }
    }
}

async fn handle_viewer_messages(
    state: ServerState,
    session_id: SessionId,
    viewer_id: ParticipantId,
    out_tx: mpsc::UnboundedSender<WsMessage>,
    mut incoming: futures_util::stream::SplitStream<WebSocket>,
) {
    while let Some(Ok(WsMessage::Text(message))) = incoming.next().await {
        let Ok(message) = viewer_protocol::UpstreamMessage::from_json(&message) else {
            continue;
        };

        match message {
            viewer_protocol::UpstreamMessage::Ping { data } => {
                let _ =
                    send_viewer_message(&out_tx, viewer_protocol::DownstreamMessage::Pong { data });
            }
            viewer_protocol::UpstreamMessage::UpdateSelection(update) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                if let Some(viewer) = session.viewers.get_mut(&viewer_id) {
                    viewer.selection = update.selection.clone();
                }
                let presence = ParticipantPresenceUpdate {
                    participant_id: viewer_id.clone(),
                    update: PresenceUpdate::Selection(update.selection),
                };
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::ParticipantPresenceUpdated(
                        presence.clone(),
                    ),
                );
                if let Some(sharer_tx) = &session.sharer_tx {
                    let _ = send_sharer_message(
                        sharer_tx,
                        sharer_protocol::DownstreamMessage::ParticipantPresenceUpdated(presence),
                    );
                }
            }
            viewer_protocol::UpstreamMessage::RequestRole(role) => {
                handle_viewer_role_request(&state, session_id, &viewer_id, role, &out_tx).await;
            }
            viewer_protocol::UpstreamMessage::UpdateInput(update) => {
                forward_viewer_input_update(&state, session_id, &viewer_id, update, &out_tx).await;
            }
            viewer_protocol::UpstreamMessage::ExecuteCommand { buffer_id, command } => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                let id = CommandExecutionRequestId::new();
                if viewer_can_execute(session, &viewer_id) {
                    let participant_id = viewer_id.clone();
                    if let Some(sharer_tx) = &session.sharer_tx {
                        let _ = send_sharer_message(
                            sharer_tx,
                            sharer_protocol::DownstreamMessage::CommandExecutionRequested {
                                id: id.clone(),
                                participant_id,
                                buffer_id,
                                command,
                            },
                        );
                    }
                    let _ = send_viewer_message(
                        &out_tx,
                        viewer_protocol::DownstreamMessage::CommandExecutionRequestInFlight(id),
                    );
                } else {
                    let _ = send_viewer_message(
                        &out_tx,
                        viewer_protocol::DownstreamMessage::CommandExecutionRequestFailed {
                            id,
                            reason: CommandExecutionFailureReason::InsufficientPermissions,
                        },
                    );
                }
            }
            viewer_protocol::UpstreamMessage::WriteToPty { request_id, bytes } => {
                let sessions = state.shared_sessions.read().await;
                let Some(session) = sessions.get(&session_id) else {
                    break;
                };
                if viewer_can_execute(session, &viewer_id) {
                    if let Some(sharer_tx) = &session.sharer_tx {
                        let _ = send_sharer_message(
                            sharer_tx,
                            sharer_protocol::DownstreamMessage::WriteToPtyRequested {
                                id: request_id,
                                bytes,
                            },
                        );
                    }
                } else {
                    let _ = send_viewer_message(
                        &out_tx,
                        viewer_protocol::DownstreamMessage::WriteToPtyRequestFailed {
                            reason: WriteToPtyFailureReason::InsufficientPermissions,
                        },
                    );
                }
            }
            viewer_protocol::UpstreamMessage::SendAgentPrompt(request) => {
                let request_id = request.id.clone();
                let (can_execute, sharer_tx, agent_run_id) = {
                    let sessions = state.shared_sessions.read().await;
                    let Some(session) = sessions.get(&session_id) else {
                        break;
                    };
                    (
                        viewer_can_execute(session, &viewer_id),
                        session.sharer_tx.clone(),
                        session.agent_run_id.clone(),
                    )
                };
                if can_execute {
                    if let Some(agent_run_id) = agent_run_id {
                        let result = cloud_agent::submit_shared_session_followup(
                            state.clone(),
                            agent_run_id,
                            session_id.clone(),
                            request.prompt.clone(),
                        )
                        .await;
                        match result {
                            Ok(()) => {
                                let _ = send_viewer_message(
                                    &out_tx,
                                    viewer_protocol::DownstreamMessage::AgentPromptRequestInFlight(
                                        request_id.clone(),
                                    ),
                                );
                            }
                            Err(err) => {
                                let _ = send_viewer_message(
                                    &out_tx,
                                    viewer_protocol::DownstreamMessage::AgentPromptRequestFailed {
                                        reason: err.agent_prompt_failure_reason(),
                                    },
                                );
                            }
                        }
                        continue;
                    } else if let Some(sharer_tx) = &sharer_tx {
                        let _ = send_sharer_message(
                            sharer_tx,
                            sharer_protocol::DownstreamMessage::AgentPromptRequested {
                                id: request_id.clone(),
                                participant_id: viewer_id.clone(),
                                request,
                            },
                        );
                    }
                    let _ = send_viewer_message(
                        &out_tx,
                        viewer_protocol::DownstreamMessage::AgentPromptRequestInFlight(request_id),
                    );
                } else {
                    let _ = send_viewer_message(
                        &out_tx,
                        viewer_protocol::DownstreamMessage::AgentPromptRequestFailed {
                            reason: session_sharing_protocol::common::AgentPromptFailureReason::InsufficientPermissions,
                        },
                    );
                }
            }
            viewer_protocol::UpstreamMessage::SendControlAction(action) => {
                let sessions = state.shared_sessions.read().await;
                let Some(session) = sessions.get(&session_id) else {
                    break;
                };
                if viewer_can_execute(session, &viewer_id) {
                    if let Some(sharer_tx) = &session.sharer_tx {
                        let _ = send_sharer_message(
                            sharer_tx,
                            sharer_protocol::DownstreamMessage::ControlActionRequested {
                                participant_id: viewer_id.clone(),
                                request_id: ControlActionRequestId::new(),
                                action,
                            },
                        );
                    }
                } else {
                    let _ = send_viewer_message(
                        &out_tx,
                        viewer_protocol::DownstreamMessage::ControlActionRequestFailed {
                            reason: ControlActionFailureReason::InsufficientPermissions,
                        },
                    );
                }
            }
            viewer_protocol::UpstreamMessage::UpdateUniversalDeveloperInputContext(update) => {
                let mut sessions = state.shared_sessions.write().await;
                let Some(session) = sessions.get_mut(&session_id) else {
                    break;
                };
                let current = session
                    .universal_developer_input_context
                    .take()
                    .unwrap_or_default();
                session.universal_developer_input_context =
                    Some(update.clone().merge_into(current));
                if let Some(sharer_tx) = &session.sharer_tx {
                    let _ = send_sharer_message(
                        sharer_tx,
                        sharer_protocol::DownstreamMessage::UniversalDeveloperInputContextUpdated(
                            update.clone(),
                        ),
                    );
                }
                fanout_viewers(
                    session,
                    viewer_protocol::DownstreamMessage::UniversalDeveloperInputContextUpdated(
                        update,
                    ),
                );
            }
            viewer_protocol::UpstreamMessage::ReportTerminalSize { window_size } => {
                let sessions = state.shared_sessions.read().await;
                let Some(session) = sessions.get(&session_id) else {
                    break;
                };
                if let Some(sharer_tx) = &session.sharer_tx {
                    let _ = send_sharer_message(
                        sharer_tx,
                        sharer_protocol::DownstreamMessage::ViewerTerminalSizeReported {
                            participant_id: viewer_id.clone(),
                            window_size,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    let mut sessions = state.shared_sessions.write().await;
    if let Some(session) = sessions.get_mut(&session_id) {
        if session
            .viewers
            .get(&viewer_id)
            .is_some_and(|viewer| viewer.tx.same_channel(&out_tx))
        {
            session.viewers.remove(&viewer_id);
            let participant_list = participant_list(session);
            fanout_participant_list(session, participant_list);
        }
    }
}

async fn handle_viewer_role_request(
    state: &ServerState,
    session_id: SessionId,
    viewer_id: &ParticipantId,
    role: Role,
    out_tx: &mpsc::UnboundedSender<WsMessage>,
) {
    let mut sessions = state.shared_sessions.write().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        return;
    };
    let can_take_control = !role.can_execute()
        || session
            .viewers
            .iter()
            .all(|(id, viewer)| id == viewer_id || !viewer.role.can_execute());

    if can_take_control {
        if let Some(viewer) = session.viewers.get_mut(viewer_id) {
            viewer.role = role;
        }
        let participant_list = participant_list(session);
        fanout_participant_list(session, participant_list);
        let response = RoleRequestResponse::Approved { new_role: role };
        let _ = send_viewer_message(
            out_tx,
            viewer_protocol::DownstreamMessage::RoleRequestResponse(response),
        );
        fanout_viewers(
            session,
            viewer_protocol::DownstreamMessage::ParticipantRoleChanged {
                participant_id: viewer_id.clone(),
                reason: RoleUpdatedReason::UpdatedBySharer,
                role,
            },
        );
    } else {
        let _ = send_viewer_message(
            out_tx,
            viewer_protocol::DownstreamMessage::RoleRequestResponse(
                RoleRequestResponse::Rejected {
                    reason: RoleRequestRejectedReason::RejectedBySharer,
                },
            ),
        );
    }
}

async fn forward_viewer_input_update(
    state: &ServerState,
    session_id: SessionId,
    viewer_id: &ParticipantId,
    update: InputUpdate,
    out_tx: &mpsc::UnboundedSender<WsMessage>,
) {
    let sessions = state.shared_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        return;
    };
    if !viewer_can_execute(session, viewer_id) {
        let _ = send_viewer_message(
            out_tx,
            viewer_protocol::DownstreamMessage::InputUpdateRejected {
                id: update.id,
                reason: session_sharing_protocol::common::InputUpdateFailureReason::InsufficientPermissions,
            },
        );
        return;
    }
    if let Some(sharer_tx) = &session.sharer_tx {
        let _ = send_sharer_message(
            sharer_tx,
            sharer_protocol::DownstreamMessage::InputUpdated(update),
        );
    }
}

fn viewer_can_execute(session: &SharedSession, viewer_id: &ParticipantId) -> bool {
    session
        .viewers
        .get(viewer_id)
        .is_some_and(|viewer| viewer.role.can_execute())
}

fn events_after(
    session: &SharedSession,
    last_received_event_no: Option<usize>,
) -> Vec<OrderedTerminalEvent> {
    session
        .events
        .iter()
        .filter(|(event_no, _)| last_received_event_no.map_or(true, |last| **event_no > last))
        .map(|(_, event)| event.clone())
        .collect()
}

fn participant_list(session: &SharedSession) -> ParticipantList {
    let sharer_info = ParticipantInfo {
        id: session.sharer_id.clone(),
        profile_data: ProfileData {
            firebase_uid: session.sharer_firebase_uid.clone(),
            display_name: session.sharer_display_name.clone(),
            photo_url: None,
            email: None,
            input_replica_id: session.input_replica_id.clone(),
        },
        selection: session.sharer_selection.clone(),
    };

    let viewers = session
        .viewers
        .iter()
        .map(|(id, viewer)| Viewer {
            info: viewer_info(id, viewer),
            role: viewer.role,
            is_present: true,
        })
        .collect::<Vec<_>>();
    let present_viewers = session
        .viewers
        .iter()
        .map(|(id, viewer)| PresentViewer {
            info: viewer_info(id, viewer),
            max_acl: viewer.role,
        })
        .collect::<Vec<_>>();

    ParticipantList {
        sharer: session_sharing_protocol::common::Sharer { info: sharer_info },
        viewers,
        present_viewers,
        absent_viewers: Vec::<AbsentViewer>::new(),
        guests: Vec::new(),
        pending_guests: Vec::new(),
    }
}

fn viewer_info(id: &ParticipantId, viewer: &ViewerState) -> ParticipantInfo {
    ParticipantInfo {
        id: id.clone(),
        profile_data: ProfileData {
            firebase_uid: viewer.firebase_uid.clone(),
            display_name: viewer.display_name.clone(),
            photo_url: None,
            email: None,
            input_replica_id: InputReplicaId::default(),
        },
        selection: viewer.selection.clone(),
    }
}

fn fanout_participant_list(session: &SharedSession, participant_list: ParticipantList) {
    fanout_viewers(
        session,
        viewer_protocol::DownstreamMessage::ParticipantListUpdated(participant_list.clone()),
    );
    if let Some(sharer_tx) = &session.sharer_tx {
        let _ = send_sharer_message(
            sharer_tx,
            sharer_protocol::DownstreamMessage::ParticipantListUpdated(participant_list),
        );
    }
}

fn fanout_viewers(session: &SharedSession, message: viewer_protocol::DownstreamMessage) {
    for viewer in session.viewers.values() {
        let _ = send_viewer_message(&viewer.tx, message.clone());
    }
}

fn send_sharer_message(
    tx: &mpsc::UnboundedSender<WsMessage>,
    message: sharer_protocol::DownstreamMessage,
) -> Result<(), mpsc::error::SendError<WsMessage>> {
    let text = message
        .to_json()
        .unwrap_or_else(|err| json!({ "error": err.to_string() }).to_string());
    tx.send(WsMessage::Text(text.into()))
}

fn send_viewer_message(
    tx: &mpsc::UnboundedSender<WsMessage>,
    message: viewer_protocol::DownstreamMessage,
) -> Result<(), mpsc::error::SendError<WsMessage>> {
    let text = message
        .to_json()
        .unwrap_or_else(|err| json!({ "error": err.to_string() }).to_string());
    tx.send(WsMessage::Text(text.into()))
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
        "GetRequestLimitInfo" | "getRequestLimitInfo" => get_request_limit_info_response(),
        "ListAIConversationMetadata"
        | "listAIConversationMetadata"
        | "ListAIConversations"
        | "listAIConversations" => list_ai_conversations_response(),
        "UpdateAgentTask" | "updateAgentTask" => update_agent_task_response(),
        "GetCloudEnvironments" | "getCloudEnvironments" => {
            get_cloud_environments_response(&state).await
        }
        "GetUpdatedCloudObjects" | "getUpdatedCloudObjects" => {
            get_updated_cloud_objects_response(&state, &body).await
        }
        "GetWorkspacesMetadataForUser" | "getWorkspacesMetadataForUser" => {
            get_workspaces_metadata_for_user_response()
        }
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
                .into_response();
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

    let workspace = workspace_for_request(request);
    let messages = openai_messages_for_request(request, &model);
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

async fn generate_worker_agent_output_with_progress<OnToolCall, OnToolResult>(
    state: &ServerState,
    prompt: &str,
    context_messages: &[Value],
    workspace: &Path,
    mut on_tool_call: OnToolCall,
    mut on_tool_result: OnToolResult,
) -> Result<LocalAgentRun>
where
    OnToolCall: FnMut(&LocalToolCall) + Send,
    OnToolResult: FnMut(&LocalToolEvent) + Send,
{
    let model = LocalLlmConfig::load()?.active_model()?;

    call_openai_compatible_autonomous_with_progress(
        &state.client,
        &model,
        openai_messages_from_worker_context(context_messages, prompt, &model),
        workspace,
        &mut on_tool_call,
        &mut on_tool_result,
    )
    .await
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

async fn call_openai_compatible_with_progress<OnToolCall, OnToolResult>(
    _client: &reqwest::Client,
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
    agent_providers::run_once_with_progress(
        model,
        messages,
        workspace,
        on_tool_call,
        on_tool_result,
    )
    .await
}

async fn call_openai_compatible_autonomous_with_progress<OnToolCall, OnToolResult>(
    _client: &reqwest::Client,
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
    agent_providers::run_autonomous_with_progress(
        model,
        messages,
        workspace,
        on_tool_call,
        on_tool_result,
    )
    .await
}

fn thinking_enabled(thinking: &str) -> bool {
    !thinking.trim().is_empty() && !thinking.trim().eq_ignore_ascii_case("off")
}

fn thinking_explicitly_configured(thinking: &str) -> bool {
    !thinking.trim().is_empty()
        && !thinking.trim().eq_ignore_ascii_case("auto")
        && !thinking.trim().eq_ignore_ascii_case("off")
}

#[derive(Clone, Copy)]
struct LocalToolDescriptor {
    name: &'static str,
    description: &'static str,
    parameters: fn() -> Value,
    execute: fn(&LocalToolCall, &Path) -> Result<LocalToolResult>,
}

const LOCAL_TOOL_REGISTRY: &[LocalToolDescriptor] = &[
    LocalToolDescriptor {
        name: "read_file",
        description: "Read a UTF-8 text file from the current workspace. The path must be relative to the workspace.",
        parameters: local_tool_read_file_parameters,
        execute: execute_read_file_tool,
    },
    LocalToolDescriptor {
        name: "write_file",
        description: "Create or overwrite a UTF-8 text file in the current workspace. The path must be relative to the workspace. Existing files require overwrite=true.",
        parameters: local_tool_write_file_parameters,
        execute: execute_write_file_tool,
    },
    LocalToolDescriptor {
        name: "search_replace",
        description: "Make a targeted edit in an existing UTF-8 file by replacing an exact text block. The search text must match exactly once.",
        parameters: local_tool_search_replace_parameters,
        execute: execute_search_replace_tool,
    },
    LocalToolDescriptor {
        name: "grep",
        description: "Search workspace files for a Rust-regex pattern. The path must be relative to the workspace.",
        parameters: local_tool_grep_parameters,
        execute: execute_grep_tool,
    },
    LocalToolDescriptor {
        name: "bash",
        description: "Run a non-interactive shell command in the current workspace. Prefer read_file, grep, and write_file for file operations.",
        parameters: local_tool_bash_parameters,
        execute: execute_bash_tool,
    },
];

fn local_openai_tools(model: &ResolvedLocalLlm) -> Vec<Value> {
    let enabled = model.enabled_tools();
    LOCAL_TOOL_REGISTRY
        .iter()
        .filter(|tool| {
            enabled
                .as_ref()
                .map_or(true, |enabled| enabled.contains(tool.name))
        })
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": (tool.parameters)(),
                }
            })
        })
        .collect()
}

fn local_tool_read_file_parameters() -> Value {
    json!({
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
    })
}

fn local_tool_write_file_parameters() -> Value {
    json!({
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
    })
}

fn local_tool_search_replace_parameters() -> Value {
    json!({
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
    })
}

fn local_tool_grep_parameters() -> Value {
    json!({
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
    })
}

fn local_tool_bash_parameters() -> Value {
    json!({
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
    })
}

fn execute_local_tool(tool_call: &LocalToolCall, workspace: &Path) -> Result<LocalToolResult> {
    let descriptor = LOCAL_TOOL_REGISTRY
        .iter()
        .find(|tool| tool.name == tool_call.name)
        .context("unsupported local tool")?;
    (descriptor.execute)(tool_call, workspace)
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

#[derive(Default)]
struct PendingOpenAiAssistantTurn {
    request_id: Option<String>,
    content: Vec<String>,
    tool_calls: Vec<LocalToolCall>,
}

impl PendingOpenAiAssistantTurn {
    fn is_empty(&self) -> bool {
        self.content.is_empty() && self.tool_calls.is_empty()
    }

    fn accepts_request_id(&self, request_id: &str) -> bool {
        self.request_id.as_deref().map_or(true, |existing| {
            !request_id.is_empty() && existing == request_id
        })
    }

    fn set_request_id_if_needed(&mut self, request_id: &str) {
        if self.request_id.is_none() && !request_id.is_empty() {
            self.request_id = Some(request_id.to_string());
        }
    }

    fn push_content(&mut self, request_id: &str, content: &str, messages: &mut Vec<Value>) {
        if !self.is_empty() && !self.accepts_request_id(request_id) {
            self.flush(messages);
        }
        self.set_request_id_if_needed(request_id);
        self.content.push(content.to_string());
    }

    fn push_tool_call(
        &mut self,
        request_id: &str,
        tool_call: LocalToolCall,
        messages: &mut Vec<Value>,
    ) {
        if !self.is_empty() && !self.accepts_request_id(request_id) {
            self.flush(messages);
        }
        self.set_request_id_if_needed(request_id);
        self.tool_calls.push(tool_call);
    }

    fn flush(&mut self, messages: &mut Vec<Value>) {
        if self.is_empty() {
            return;
        }

        let content = self.content.join("\n\n");
        if self.tool_calls.is_empty() {
            if !content.trim().is_empty() {
                messages.push(openai_assistant_text_message(&content));
            }
        } else {
            messages.push(openai_assistant_message(&LocalAssistantTurn {
                content,
                reasoning: String::new(),
                tool_calls: std::mem::take(&mut self.tool_calls),
            }));
        }

        self.request_id = None;
        self.content.clear();
    }
}

fn openai_messages_for_request(request: &maa::Request, model: &ResolvedLocalLlm) -> Vec<Value> {
    let mut messages = vec![openai_system_message(model)];
    let mut local_tool_call_ids = std::collections::HashSet::new();
    let mut local_tool_result_ids = std::collections::HashSet::new();
    let mut pending_assistant = PendingOpenAiAssistantTurn::default();

    if let Some(task_context) = request.task_context.as_ref() {
        for task in &task_context.tasks {
            for message in &task.messages {
                match message.message.as_ref() {
                    Some(maa::message::Message::UserQuery(query)) => {
                        pending_assistant.flush(&mut messages);
                        if !query.query.trim().is_empty() {
                            messages.push(openai_user_message(&query.query));
                        }
                    }
                    Some(maa::message::Message::AgentOutput(output)) => {
                        if !output.text.trim().is_empty() {
                            pending_assistant.push_content(
                                &message.request_id,
                                &output.text,
                                &mut messages,
                            );
                        }
                    }
                    Some(maa::message::Message::ToolCall(tool_call)) => {
                        if let Some(local_tool_call) = local_tool_call_from_api(tool_call) {
                            local_tool_call_ids.insert(local_tool_call.id.clone());
                            pending_assistant.push_tool_call(
                                &message.request_id,
                                local_tool_call,
                                &mut messages,
                            );
                        }
                    }
                    Some(maa::message::Message::ToolCallResult(result))
                        if local_tool_call_ids.contains(&result.tool_call_id) =>
                    {
                        pending_assistant.flush(&mut messages);
                        local_tool_result_ids.insert(result.tool_call_id.clone());
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
    pending_assistant.flush(&mut messages);

    let mut added_current_tool_result = false;
    for result in current_tool_results_for_request(request) {
        if local_tool_call_ids.contains(&result.tool_call_id)
            && local_tool_result_ids.insert(result.tool_call_id.clone())
        {
            messages.push(openai_tool_result_message(&result));
            added_current_tool_result = true;
        }
    }

    if let Some(prompt) = extract_user_prompt(request).filter(|prompt| !prompt.trim().is_empty()) {
        messages.push(openai_user_message(&prompt));
    } else if !added_current_tool_result {
        messages.push(openai_user_message(
            "Continue the current Warp agent conversation.",
        ));
    }
    messages
}

#[allow(deprecated)]
fn current_tool_results_for_request(request: &maa::Request) -> Vec<LocalToolResult> {
    use maa::request::input::user_inputs::user_input::Input as UserInput;
    use maa::request::input::Type;

    let Some(input) = request.input.as_ref() else {
        return Vec::new();
    };
    let Some(Type::UserInputs(inputs)) = input.r#type.as_ref() else {
        return Vec::new();
    };

    inputs
        .inputs
        .iter()
        .filter_map(|input| match input.input.as_ref()? {
            UserInput::ToolCallResult(result) => Some(LocalToolResult {
                tool_call_id: result.tool_call_id.clone(),
                name: "tool".to_string(),
                content: request_tool_result_text(result),
            }),
            _ => None,
        })
        .collect()
}

fn openai_messages_from_worker_context(
    context_messages: &[Value],
    prompt: &str,
    model: &ResolvedLocalLlm,
) -> Vec<Value> {
    let mut messages = vec![openai_system_message(model)];
    messages.extend(normalize_openai_context_messages(context_messages));
    messages.push(openai_user_message(prompt));
    messages
}

fn openai_worker_context_for_prompt(context_messages: &[Value], prompt: &str) -> Vec<Value> {
    let mut messages = normalize_openai_context_messages(context_messages);
    messages.push(openai_user_message(prompt));
    messages
}

fn append_openai_worker_context_output(context_messages: &mut Vec<Value>, output: &str) {
    if !output.trim().is_empty() {
        context_messages.push(openai_assistant_text_message(output));
    }
}

pub(crate) fn append_openai_worker_context_tool_call(
    context_messages: &mut Vec<Value>,
    tool_call: &LocalToolCall,
) {
    context_messages.push(json!({
        "role": "assistant",
        "tool_calls": [openai_tool_call_message(tool_call)],
    }));
}

pub(crate) fn append_openai_worker_context_tool_result(
    context_messages: &mut Vec<Value>,
    result: &LocalToolResult,
) {
    context_messages.push(openai_tool_result_message(result));
}

fn normalize_openai_context_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| {
            let role = message.get("role").and_then(Value::as_str)?;
            match role {
                "user" => normalize_openai_user_context_message(message),
                "assistant" => normalize_openai_assistant_context_message(message),
                "tool" => normalize_openai_tool_context_message(message),
                _ => None,
            }
        })
        .collect()
}

fn normalize_openai_user_context_message(message: &Value) -> Option<Value> {
    let content = message
        .get("content")
        .and_then(openai_context_content_text)
        .filter(|content| !content.trim().is_empty())?;

    Some(json!({
        "role": "user",
        "content": content,
    }))
}

fn normalize_openai_assistant_context_message(message: &Value) -> Option<Value> {
    let content = message
        .get("content")
        .and_then(openai_context_content_text)
        .unwrap_or_default();
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|tool_calls| !tool_calls.is_empty())
        .cloned();

    if content.trim().is_empty() && tool_calls.is_none() {
        return None;
    }

    let mut normalized = serde_json::Map::new();
    normalized.insert("role".to_string(), Value::String("assistant".to_string()));
    if !content.trim().is_empty() {
        normalized.insert("content".to_string(), Value::String(content));
    }
    if let Some(tool_calls) = tool_calls {
        normalized.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Some(Value::Object(normalized))
}

fn normalize_openai_tool_context_message(message: &Value) -> Option<Value> {
    let tool_call_id = message
        .get("tool_call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tool_call_id| !tool_call_id.is_empty())?;
    let content = message
        .get("content")
        .and_then(openai_context_content_text)
        .filter(|content| !content.trim().is_empty())
        .unwrap_or_default();
    let name = message
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");

    Some(json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "name": name,
        "content": content,
    }))
}

fn openai_context_content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.to_owned()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.as_str())
                })
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn openai_system_message(model: &ResolvedLocalLlm) -> Value {
    json!({
        "role": "system",
        "content": local_agent_system_prompt(model)
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
            run_shell_command_result_text(result)
        }
        Some(maa::message::tool_call_result::Result::ReadFiles(result)) => {
            read_files_result_text(result)
        }
        Some(maa::message::tool_call_result::Result::Grep(result)) => grep_result_text(result),
        Some(maa::message::tool_call_result::Result::ApplyFileDiffs(result)) => {
            apply_file_diffs_tool_result_text(result)
        }
        _ => "Tool result is unavailable.".to_string(),
    }
}

#[allow(deprecated)]
fn request_tool_result_text(result: &maa::request::input::ToolCallResult) -> String {
    match result.result.as_ref() {
        Some(maa::request::input::tool_call_result::Result::RunShellCommand(result)) => {
            run_shell_command_result_text(result)
        }
        Some(maa::request::input::tool_call_result::Result::ReadFiles(result)) => {
            read_files_result_text(result)
        }
        Some(maa::request::input::tool_call_result::Result::Grep(result)) => {
            grep_result_text(result)
        }
        Some(maa::request::input::tool_call_result::Result::ApplyFileDiffs(result)) => {
            apply_file_diffs_tool_result_text(result)
        }
        _ => "Tool result is unavailable.".to_string(),
    }
}

fn run_shell_command_result_text(result: &maa::RunShellCommandResult) -> String {
    match result.result.as_ref() {
        Some(maa::run_shell_command_result::Result::CommandFinished(finished)) => format!(
            "Command: {}\nExit code: {}\n\n{}",
            result.command,
            finished.exit_code,
            strip_local_command_header(&finished.output)
        ),
        Some(maa::run_shell_command_result::Result::PermissionDenied(_)) => {
            format!("Command was not approved: {}", result.command)
        }
        _ => format!("Command result for {} is unavailable.", result.command),
    }
}

fn read_files_result_text(result: &maa::ReadFilesResult) -> String {
    match result.result.as_ref() {
        Some(maa::read_files_result::Result::TextFilesSuccess(success)) => success
            .files
            .iter()
            .map(file_content_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        Some(maa::read_files_result::Result::AnyFilesSuccess(success)) => {
            let files = success
                .files
                .iter()
                .map(any_file_content_text)
                .collect::<Vec<_>>()
                .join("\n\n");
            if files.is_empty() {
                format!("Read {} file(s).", success.files.len())
            } else {
                files
            }
        }
        Some(maa::read_files_result::Result::Error(error)) => error.message.clone(),
        None => "Read file result is unavailable.".to_string(),
    }
}

fn file_content_text(file: &maa::FileContent) -> String {
    format!("{}:\n{}", file.file_path, file.content)
}

fn any_file_content_text(file: &maa::AnyFileContent) -> String {
    match file.content.as_ref() {
        Some(maa::any_file_content::Content::TextContent(file)) => file_content_text(file),
        Some(maa::any_file_content::Content::BinaryContent(file)) => {
            format!("{}:\n<binary file>", file.file_path)
        }
        None => "Unknown file content.".to_string(),
    }
}

fn grep_result_text(result: &maa::GrepResult) -> String {
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

fn apply_file_diffs_tool_result_text(result: &maa::ApplyFileDiffsResult) -> String {
    match result.result.as_ref() {
        Some(maa::apply_file_diffs_result::Result::Success(success)) => {
            apply_file_diffs_result_text(success)
        }
        Some(maa::apply_file_diffs_result::Result::Error(error)) => error.message.clone(),
        None => "File edit result is unavailable.".to_string(),
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

const DEFAULT_LOCAL_AGENT_SYSTEM_PROMPT: &str = "You are a local coding agent running inside a Warp OSS loopback sidecar. \
Use tools to inspect and modify the user's current workspace. \
Available tools: {tools}. \
Before editing an existing file, inspect it with read_file or grep. \
Prefer read_file, grep, search_replace, and write_file over bash for file operations. \
After making code changes, run a relevant verification command with bash when one is reasonably available. \
Keep final answers concise and report what changed plus any verification result.";

fn local_agent_system_prompt(model: &ResolvedLocalLlm) -> String {
    let tool_names = local_openai_tools(model)
        .into_iter()
        .filter_map(|tool| {
            tool.pointer("/function/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let tools = if tool_names.is_empty() {
        "no tools".to_string()
    } else {
        tool_names.join(", ")
    };
    let configured = model
        .configured_system_prompt()
        .unwrap_or(DEFAULT_LOCAL_AGENT_SYSTEM_PROMPT);
    if configured.contains("{tools}") {
        configured.replace("{tools}", &tools)
    } else {
        format!("{configured}\nAvailable tools: {tools}.")
    }
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

fn agent_reasoning_message(reasoning: &str, task_id: &str, request_id: &str) -> maa::Message {
    local_message(
        task_id,
        request_id,
        maa::message::Message::AgentReasoning(maa::message::AgentReasoning {
            reasoning: reasoning.to_string(),
            finished_duration: None,
        }),
    )
}

fn user_query_message(query: &str, task_id: &str, request_id: &str) -> maa::Message {
    local_message(
        task_id,
        request_id,
        maa::message::Message::UserQuery(maa::message::UserQuery {
            query: query.to_string(),
            context: None,
            referenced_attachments: HashMap::new(),
            mode: None,
            intended_agent: Default::default(),
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
        if coordinator::try_proxy_multi_agent_to_worker(state.clone(), request.clone(), tx.clone())
            .await
        {
            return;
        }

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

        let mut final_messages = Vec::new();
        if !run.reasoning.trim().is_empty() {
            final_messages.push(agent_reasoning_message(
                &run.reasoning,
                &task_info.id,
                &stream_ids.request_id,
            ));
        }
        if !run.output.trim().is_empty() {
            final_messages.push(agent_output_message(
                &run.output,
                &task_info.id,
                &stream_ids.request_id,
            ));
        }
        if !final_messages.is_empty() {
            send_response_event(&tx, add_messages_event(&task_info.id, final_messages));
        }
        send_response_event(&tx, finished_event());
    });

    response_event_receiver_stream(rx)
}

#[cfg(test)]
fn agent_response_events(request: &maa::Request, run: LocalAgentRun) -> Vec<maa::ResponseEvent> {
    let stream_ids = stream_ids(request);
    let task_info = task_info(request);
    let mut messages = Vec::new();
    if !run.reasoning.trim().is_empty() {
        messages.push(agent_reasoning_message(
            &run.reasoning,
            &task_info.id,
            &stream_ids.request_id,
        ));
    }
    for tool_call in &run.tool_calls {
        messages.push(local_tool_call_message(
            tool_call,
            &task_info.id,
            &stream_ids.request_id,
        ));
    }
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
    if !run.output.trim().is_empty() {
        messages.push(agent_output_message(
            &run.output,
            &task_info.id,
            &stream_ids.request_id,
        ));
    }

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

fn cloud_agent_initial_events(run_id: &str, title: &str, prompt: &str) -> Vec<maa::ResponseEvent> {
    let stream_ids = cloud_agent_stream_ids(run_id);
    let task_info = cloud_agent_task_info(run_id, title, prompt);
    let mut actions = vec![create_task_action(&task_info)];
    if !prompt.trim().is_empty() {
        actions.push(add_messages_action(
            &task_info.id,
            vec![user_query_message(
                prompt,
                &task_info.id,
                &stream_ids.request_id,
            )],
        ));
    }

    vec![init_event(&stream_ids), client_actions_event(actions)]
}

fn cloud_agent_followup_initial_events(run_id: &str, prompt: &str) -> Vec<maa::ResponseEvent> {
    let stream_ids = cloud_agent_stream_ids(run_id);
    let mut events = vec![init_event(&stream_ids)];
    if !prompt.trim().is_empty() {
        events.push(add_messages_event(
            run_id,
            vec![user_query_message(prompt, run_id, &stream_ids.request_id)],
        ));
    }
    events
}

fn cloud_agent_output_event(run_id: &str, output: &str) -> maa::ResponseEvent {
    let request_id = cloud_agent_request_id(run_id);
    add_messages_event(
        run_id,
        vec![agent_output_message(output, run_id, &request_id)],
    )
}

fn cloud_agent_reasoning_event(run_id: &str, reasoning: &str) -> maa::ResponseEvent {
    let request_id = cloud_agent_request_id(run_id);
    add_messages_event(
        run_id,
        vec![agent_reasoning_message(reasoning, run_id, &request_id)],
    )
}

fn cloud_agent_finished_event() -> maa::ResponseEvent {
    finished_event()
}

fn cloud_agent_stream_ids(run_id: &str) -> StreamIds {
    StreamIds {
        conversation_id: format!("warpsolo-cloud-conversation-{run_id}"),
        request_id: cloud_agent_request_id(run_id),
        run_id: run_id.to_string(),
    }
}

fn cloud_agent_request_id(run_id: &str) -> String {
    format!("warpsolo-cloud-request-{run_id}")
}

fn cloud_agent_task_info(run_id: &str, title: &str, prompt: &str) -> TaskInfo {
    TaskInfo {
        id: run_id.to_string(),
        description: title
            .trim()
            .lines()
            .next()
            .filter(|title| !title.is_empty())
            .or_else(|| prompt.trim().lines().next())
            .unwrap_or("WarpSOLO agent")
            .to_string(),
        needs_create: true,
    }
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

fn get_request_limit_info_response() -> Value {
    json!({
        "data": {
            "user": {
                "__typename": "UserOutput",
                "user": {
                    "workspaces": [],
                    "requestLimitInfo": {
                        "isUnlimited": true,
                        "requestsUsedSinceLastRefresh": 0,
                        "requestLimit": 1_000_000,
                        "nextRefreshTime": "2099-01-01T00:00:00Z",
                        "requestLimitRefreshDuration": "MONTHLY",
                        "isUnlimitedVoice": true,
                        "voiceRequestLimit": 1_000_000,
                        "voiceRequestsUsedSinceLastRefresh": 0,
                        "isUnlimitedCodebaseIndices": true,
                        "maxCodebaseIndices": 1_000_000,
                        "maxFilesPerRepo": 1_000_000,
                        "embeddingGenerationBatchSize": 100,
                    },
                    "bonusGrants": [],
                },
            },
        },
    })
}

fn list_ai_conversations_response() -> Value {
    json!({
        "data": {
            "listAIConversations": {
                "__typename": "ListAIConversationsOutput",
                "conversations": [],
                "responseContext": response_context(),
            },
        },
    })
}

fn update_agent_task_response() -> Value {
    json!({
        "data": {
            "updateAgentTask": {
                "__typename": "UpdateAgentTaskOutput",
                "responseContext": response_context(),
            },
        },
    })
}

async fn get_cloud_environments_response(state: &ServerState) -> Value {
    let workers = state
        .discovered_workers
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    get_cloud_environments_response_for_workers(&state.account, &workers)
}

fn get_cloud_environments_response_for_workers(
    account: &LocalAccount,
    workers: &[worker_discovery::DiscoveredWorker],
) -> Value {
    let cloud_environments = workers
        .iter()
        .map(|worker| synthetic_peer_cloud_environment(account, worker))
        .collect::<Vec<_>>();

    json!({
        "data": {
            "getCloudEnvironments": {
                "__typename": "GetCloudEnvironmentsOutput",
                "cloudEnvironments": cloud_environments,
                "responseContext": response_context(),
            },
        },
    })
}

async fn get_updated_cloud_objects_response(state: &ServerState, request_body: &Value) -> Value {
    let workers = state
        .discovered_workers
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    get_updated_cloud_objects_response_for_workers(
        &state.account,
        &workers,
        requested_generic_string_object_uids(request_body),
    )
}

fn get_updated_cloud_objects_response_for_workers(
    account: &LocalAccount,
    workers: &[worker_discovery::DiscoveredWorker],
    requested_generic_string_object_uids: Vec<String>,
) -> Value {
    let current_environment_ids = workers
        .iter()
        .map(worker_discovery::synthetic_environment_id_for_worker)
        .collect::<HashSet<_>>();
    let deleted_generic_string_object_uids = requested_generic_string_object_uids
        .into_iter()
        .filter(|uid| {
            worker_discovery::is_synthetic_environment_id(uid)
                && !current_environment_ids.contains(uid)
        })
        .collect::<Vec<_>>();
    let generic_string_objects = workers
        .iter()
        .map(|worker| synthetic_peer_environment_object(account, worker))
        .collect::<Vec<_>>();

    json!({
        "data": {
            "updatedCloudObjects": {
                "__typename": "UpdatedCloudObjectsOutput",
                "actionHistories": [],
                "deletedObjectUids": {
                    "folderUids": [],
                    "genericStringObjectUids": deleted_generic_string_object_uids,
                    "notebookUids": [],
                    "workflowUids": [],
                },
                "folders": [],
                "genericStringObjects": generic_string_objects,
                "mcpGallery": [],
                "notebooks": [],
                "responseContext": response_context(),
                "userProfiles": [],
                "workflows": [],
            },
        },
    })
}

fn synthetic_peer_environment_object(
    account: &LocalAccount,
    worker: &worker_discovery::DiscoveredWorker,
) -> Value {
    let environment_id = worker_discovery::synthetic_environment_id_for_worker(worker);
    let owner_uid = stable_server_id("wuser-", &account.user_id);
    let now = chrono::Utc::now().to_rfc3339();
    let serialized_model = synthetic_peer_environment_serialized_model(worker);

    json!({
        "__typename": "GenericStringObject",
        "format": "JsonCloudEnvironment",
        "metadata": {
            "__typename": "ObjectMetadata",
            "creatorUid": null,
            "currentEditorUid": null,
            "isWelcomeObject": false,
            "lastEditorUid": null,
            "metadataLastUpdatedTs": now,
            "parent": {
                "__typename": "Space",
                "uid": owner_uid,
                "type": "User",
            },
            "revisionTs": now,
            "trashedTs": null,
            "uid": environment_id,
        },
        "permissions": {
            "__typename": "ObjectPermissions",
            "guests": [],
            "lastUpdatedTs": now,
            "anyoneLinkSharing": null,
            "space": {
                "__typename": "Space",
                "uid": owner_uid,
                "type": "User",
            },
        },
        "serializedModel": serialized_model,
    })
}

fn synthetic_peer_cloud_environment(
    account: &LocalAccount,
    worker: &worker_discovery::DiscoveredWorker,
) -> Value {
    let environment_id = worker_discovery::synthetic_environment_id_for_worker(worker);
    let owner_uid = stable_server_id("wuser-", &account.user_id);
    let now = chrono::Utc::now().to_rfc3339();

    json!({
        "__typename": "CloudEnvironment",
        "uid": environment_id,
        "config": {
            "__typename": "CloudEnvironmentConfig",
            "name": worker.display_name,
            "description": format!("WarpSOLO peer at {}", worker.url),
            "githubRepos": [],
            "dockerImage": "warpsolo/peer",
            "setupCommands": [],
            "providers": null,
        },
        "creator": null,
        "lastEditor": null,
        "lastTaskCreated": null,
        "lastTaskRunTimestamp": null,
        "lastUpdated": now,
        "scope": {
            "__typename": "Space",
            "uid": owner_uid,
            "type": "User",
        },
        "setupFailed": false,
    })
}

fn synthetic_peer_environment_serialized_model(
    worker: &worker_discovery::DiscoveredWorker,
) -> String {
    json!({
        "name": worker.display_name,
        "description": format!("WarpSOLO peer at {}", worker.url),
        "github_repos": [],
        "docker_image": "warpsolo/peer",
        "setup_commands": [],
    })
    .to_string()
}

fn stable_server_id(prefix: &str, value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let mut id = format!("{prefix}{hash:016x}");
    id.truncate(22);
    while id.len() < 22 {
        id.push('0');
    }
    id
}

fn requested_generic_string_object_uids(request_body: &Value) -> Vec<String> {
    request_body
        .pointer("/variables/input/genericStringObjects")
        .or_else(|| request_body.pointer("/input/genericStringObjects"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|object| object.get("uid").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn get_workspaces_metadata_for_user_response() -> Value {
    json!({
        "data": {
            "user": {
                "__typename": "UserOutput",
                "user": {
                    "workspaces": [],
                    "experiments": [],
                    "discoverableTeams": [],
                },
            },
            "pricingInfo": {
                "__typename": "PricingInfoOutput",
                "pricingInfo": {
                    "plans": [],
                    "overages": {
                        "pricePerRequestUsdCents": 0,
                    },
                    "addonCreditsOptions": [],
                },
            },
        },
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
        "reasoningLevel": if thinking_enabled(&model.thinking) {
            json!(model.thinking)
        } else {
            Value::Null
        },
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

fn llm_config_path() -> PathBuf {
    warp_core::paths::config_local_dir().join(LOCAL_LLM_FILE)
}

fn resolve_agent_system_prompt(path: &str) -> Result<String> {
    let path = resolve_agent_path(path)?;
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read agent prompt file {}", path.display()))
        .map(|contents| contents.trim().to_owned())
        .map(|contents| {
            if contents.is_empty() {
                String::new()
            } else {
                contents
            }
        })
}

fn resolve_agent_path(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        anyhow::bail!("system prompt file path is empty");
    }

    let resolved = if path.starts_with("~/") {
        let Some(home) = std::env::var_os("HOME") else {
            anyhow::bail!("HOME is not set, cannot resolve {path}");
        };
        let home = PathBuf::from(home);
        if path.len() <= 2 {
            home
        } else {
            home.join(&path[2..])
        }
    } else {
        warp_core::paths::config_local_dir().join(path)
    };

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    fn env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    fn test_openai_model(name: &str) -> ResolvedLocalLlm {
        ResolvedLocalLlm {
            id: name.to_string(),
            display_name: name.to_string(),
            base_model_name: name.to_string(),
            base_url: "https://example.test/v1".to_string(),
            api_style: "openai".to_string(),
            token: "test-token".to_string(),
            token_configured: true,
            headers: Vec::new(),
            reasoning_field_name: "reasoning_content".to_string(),
            thinking: "off".to_string(),
            thinking_budget: None,
            description: None,
            system_prompt: None,
            enabled_tools: None,
        }
    }

    fn test_account() -> LocalAccount {
        LocalAccount {
            user_id: "local-user-test".to_string(),
            device_id: "local-device-test".to_string(),
            display_name: "test@localhost".to_string(),
            id_token: "id-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            custom_token: "custom-token".to_string(),
        }
    }

    fn test_worker(device_id: &str) -> worker_discovery::DiscoveredWorker {
        worker_discovery::DiscoveredWorker {
            source_id: format!("test:{device_id}"),
            device_id: device_id.to_string(),
            user_id: "local-user-peer".to_string(),
            display_name: "peer@example.local".to_string(),
            hostname: "example.local.".to_string(),
            url: "http://192.168.1.10:9109".to_string(),
            capabilities: vec!["agent".to_string()],
            auth: "none".to_string(),
            last_seen_epoch_millis: 1,
        }
    }

    fn messages_from_response_events(events: &[maa::ResponseEvent]) -> Vec<&maa::Message> {
        events
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
            .collect()
    }

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
thinking = "high"
thinking_budget = 2048
"#,
        )
        .unwrap();

        let model = config.active_model().unwrap();

        assert_eq!(model.display_name, "Qwen");
        assert_eq!(model.description(), "Qwen coding");
        assert_eq!(model.thinking, "high");
        assert_eq!(model.thinking_budget, Some(2048));
        assert!(model
            .headers
            .contains(&("X-Test-Header".to_string(), "enabled".to_string())));
        assert!(model
            .headers
            .contains(&("User-Agent".to_string(), "OpenAI/Go 3.22.0".to_string())));
    }

    #[test]
    fn local_graphql_agent_stubs_return_success_payloads() {
        let _: cynic::GraphQlResponse<warp_graphql::mutations::update_agent_task::UpdateAgentTask> =
            serde_json::from_value(update_agent_task_response()).unwrap();
        let _: cynic::GraphQlResponse<
            warp_graphql::queries::list_ai_conversations::ListAIConversationMetadata,
        > = serde_json::from_value(list_ai_conversations_response()).unwrap();
        let _: cynic::GraphQlResponse<
            warp_graphql::queries::get_request_limit_info::GetRequestLimitInfo,
        > = serde_json::from_value(get_request_limit_info_response()).unwrap();
        let _: cynic::GraphQlResponse<
            warp_graphql::queries::get_updated_cloud_objects::GetUpdatedCloudObjects,
        > = serde_json::from_value(get_updated_cloud_objects_response_for_workers(
            &test_account(),
            &[],
            Vec::new(),
        ))
        .unwrap();
        let _: cynic::GraphQlResponse<
            warp_graphql::queries::get_cloud_environments::GetCloudEnvironmentsQuery,
        > = serde_json::from_value(get_cloud_environments_response_for_workers(
            &test_account(),
            &[],
        ))
        .unwrap();
        let _: cynic::GraphQlResponse<
            warp_graphql::queries::get_workspaces_metadata_for_user::GetWorkspacesMetadataForUser,
        > = serde_json::from_value(get_workspaces_metadata_for_user_response()).unwrap();

        assert_eq!(
            update_agent_task_response()["data"]["updateAgentTask"]["__typename"],
            "UpdateAgentTaskOutput"
        );
        assert_eq!(
            list_ai_conversations_response()["data"]["listAIConversations"]["conversations"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            get_request_limit_info_response()["data"]["user"]["user"]["requestLimitInfo"]
                ["isUnlimited"],
            true
        );
        assert_eq!(
            get_updated_cloud_objects_response_for_workers(&test_account(), &[], Vec::new())
                ["data"]["updatedCloudObjects"]["deletedObjectUids"]["notebookUids"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn synthetic_peer_environments_parse_as_cloud_objects() {
        let account = test_account();
        let workers = vec![test_worker("local-device-peer")];
        let response = get_updated_cloud_objects_response_for_workers(&account, &workers, vec![]);

        let _: cynic::GraphQlResponse<
            warp_graphql::queries::get_updated_cloud_objects::GetUpdatedCloudObjects,
        > = serde_json::from_value(response.clone()).unwrap();

        let env = &response["data"]["updatedCloudObjects"]["genericStringObjects"][0];
        assert_eq!(env["format"], "JsonCloudEnvironment");
        assert_eq!(
            env["metadata"]["uid"]
                .as_str()
                .expect("synthetic environment id")
                .len(),
            22
        );
        assert!(env["serializedModel"]
            .as_str()
            .expect("serialized model")
            .contains("peer@example.local"));
    }

    #[test]
    fn synthetic_peer_environments_parse_as_cloud_environments() {
        let account = test_account();
        let workers = vec![test_worker("local-device-peer")];
        let response = get_cloud_environments_response_for_workers(&account, &workers);

        let _: cynic::GraphQlResponse<
            warp_graphql::queries::get_cloud_environments::GetCloudEnvironmentsQuery,
        > = serde_json::from_value(response.clone()).unwrap();

        let env = &response["data"]["getCloudEnvironments"]["cloudEnvironments"][0];
        assert_eq!(env["__typename"], "CloudEnvironment");
        assert_eq!(env["config"]["name"], "peer@example.local");
        assert_eq!(env["config"]["dockerImage"], "warpsolo/peer");
    }

    #[test]
    fn stale_synthetic_peer_environments_are_deleted() {
        let account = test_account();
        let stale_uid =
            worker_discovery::synthetic_environment_id_for_worker(&test_worker("stale-peer"));
        let response = get_updated_cloud_objects_response_for_workers(
            &account,
            &[],
            vec![stale_uid.clone(), "non-warpsolo-object".to_string()],
        );

        assert_eq!(
            response["data"]["updatedCloudObjects"]["deletedObjectUids"]["genericStringObjectUids"],
            json!([stale_uid])
        );
    }

    #[test]
    fn local_agent_system_prompt_describes_available_tools() {
        let model = test_openai_model("qwen");
        let prompt = local_agent_system_prompt(&model);

        assert!(prompt.contains("read_file"));
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("grep"));
        assert!(prompt.contains("bash"));
    }

    #[test]
    fn resolves_tilde_agent_prompt_path_relative_to_home() {
        let _guard = env_lock().lock().unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let prompt = tempdir.path().join("local_prompt.txt");
        fs::write(&prompt, "custom prompt").unwrap();
        let home = tempdir.path().to_string_lossy().to_string();

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let path = resolve_agent_path("~/local_prompt.txt").unwrap();
        assert_eq!(path, prompt);
        assert_eq!(
            resolve_agent_system_prompt("~/local_prompt.txt").unwrap(),
            "custom prompt"
        );

        if let Some(previous_home) = previous_home {
            std::env::set_var("HOME", previous_home);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn resolves_agent_prompt_path_relative_to_config_dir() {
        let _guard = env_lock().lock().unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let home = tempdir.path();
        let config_dir = home.join(".warp-oss");
        let prompt = config_dir.join("prompts").join("agent_prompt.txt");
        fs::create_dir_all(prompt.parent().unwrap()).unwrap();
        fs::write(&prompt, "dir prompt").unwrap();

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home);

        let path = resolve_agent_path("prompts/agent_prompt.txt").unwrap();
        assert_eq!(path, prompt);
        assert_eq!(
            resolve_agent_system_prompt("prompts/agent_prompt.txt").unwrap(),
            "dir prompt"
        );

        if let Some(previous_home) = previous_home {
            std::env::set_var("HOME", previous_home);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn local_llm_config_resolves_agent_system_prompt_and_enabled_tools() {
        let config: LocalLlmConfig = toml::from_str(
            r#"
active_model = "qwen"

[agent]
system_prompt = "Use only requested tools: {tools}"
enabled_tools = ["read_file", "grep"]

[[providers]]
name = "dashscope"
api_base = "https://example.test/v1"
api_key = "test-token"
api_style = "openai"

[[models]]
name = "qwen"
provider = "dashscope"
"#,
        )
        .unwrap();

        let model = config.active_model().unwrap();
        let prompt = local_agent_system_prompt(&model);
        let tools = local_openai_tools(&model)
            .into_iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();

        assert_eq!(tools, vec!["read_file", "grep"]);
        assert_eq!(
            model.configured_system_prompt(),
            Some("Use only requested tools: {tools}")
        );
        assert_eq!(prompt, "Use only requested tools: read_file, grep");
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

        let model = test_openai_model("qwen");
        let messages = openai_messages_for_request(&request, &model);

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

        let model = test_openai_model("qwen");
        let messages = openai_messages_for_request(&request, &model);

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
    fn openai_messages_include_current_request_tool_result() {
        let request = maa::Request {
            task_context: Some(maa::request::TaskContext {
                tasks: vec![maa::Task {
                    id: "task-1".to_string(),
                    description: "task".to_string(),
                    dependencies: None,
                    messages: vec![maa::Message {
                        id: "m1".to_string(),
                        task_id: "task-1".to_string(),
                        request_id: "r1".to_string(),
                        timestamp: None,
                        server_message_data: String::new(),
                        citations: Vec::new(),
                        message: Some(maa::message::Message::ToolCall(
                            maa::message::ToolCall {
                                tool_call_id: "call_read".to_string(),
                                tool: Some(maa::message::tool_call::Tool::ReadFiles(
                                    maa::message::tool_call::ReadFiles {
                                        files: vec![maa::message::tool_call::read_files::File {
                                            name: "notes.md".to_string(),
                                            line_ranges: Vec::new(),
                                        }],
                                    },
                                )),
                            },
                        )),
                    }],
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
                                maa::request::input::user_inputs::user_input::Input::ToolCallResult(
                                    maa::request::input::ToolCallResult {
                                        tool_call_id: "call_read".to_string(),
                                        result: Some(
                                            maa::request::input::tool_call_result::Result::ReadFiles(
                                                maa::ReadFilesResult {
                                                    result: Some(
                                                        maa::read_files_result::Result::AnyFilesSuccess(
                                                            maa::read_files_result::AnyFilesSuccess {
                                                                files: vec![maa::AnyFileContent {
                                                                    content: Some(
                                                                        maa::any_file_content::Content::TextContent(
                                                                            maa::FileContent {
                                                                                file_path: "notes.md".to_string(),
                                                                                content: "local notes".to_string(),
                                                                                line_range: None,
                                                                            },
                                                                        ),
                                                                    ),
                                                                }],
                                                            },
                                                        ),
                                                    ),
                                                },
                                            ),
                                        ),
                                    },
                                ),
                            ),
                        }],
                    },
                )),
            }),
            ..Default::default()
        };

        let model = test_openai_model("qwen");
        let messages = openai_messages_for_request(&request, &model);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_read");
        assert_eq!(messages[2]["content"], "notes.md:\nlocal notes");
    }

    #[allow(deprecated)]
    #[test]
    fn openai_messages_coalesce_tool_calls_and_assistant_text_for_same_turn() {
        let request = maa::Request {
            task_context: Some(maa::request::TaskContext {
                tasks: vec![maa::Task {
                    id: "task-1".to_string(),
                    description: "task".to_string(),
                    dependencies: None,
                    messages: vec![
                        maa::Message {
                            id: "m-user".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r-user".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::UserQuery(
                                maa::message::UserQuery {
                                    query: "inspect the files".to_string(),
                                    context: None,
                                    referenced_attachments: HashMap::new(),
                                    mode: None,
                                    intended_agent: Default::default(),
                                },
                            )),
                        },
                        maa::Message {
                            id: "m-call-read".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r-agent".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::ToolCall(
                                maa::message::ToolCall {
                                    tool_call_id: "call_read".to_string(),
                                    tool: Some(maa::message::tool_call::Tool::ReadFiles(
                                        maa::message::tool_call::ReadFiles {
                                            files: vec![
                                                maa::message::tool_call::read_files::File {
                                                    name: "notes.md".to_string(),
                                                    line_ranges: Vec::new(),
                                                },
                                            ],
                                        },
                                    )),
                                },
                            )),
                        },
                        maa::Message {
                            id: "m-call-bash".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r-agent".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::ToolCall(
                                maa::message::ToolCall {
                                    tool_call_id: "call_bash".to_string(),
                                    tool: Some(maa::message::tool_call::Tool::RunShellCommand(
                                        maa::message::tool_call::RunShellCommand {
                                            command: "ls".to_string(),
                                            is_read_only: true,
                                            uses_pager: false,
                                            citations: Vec::new(),
                                            is_risky: false,
                                            risk_category: 0,
                                            wait_until_complete_value: None,
                                        },
                                    )),
                                },
                            )),
                        },
                        maa::Message {
                            id: "m-output".to_string(),
                            task_id: "task-1".to_string(),
                            request_id: "r-agent".to_string(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: Vec::new(),
                            message: Some(maa::message::Message::AgentOutput(
                                maa::message::AgentOutput {
                                    text: "Let me check that.".to_string(),
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
                        inputs: vec![
                            maa::request::input::user_inputs::UserInput {
                                input: Some(
                                    maa::request::input::user_inputs::user_input::Input::ToolCallResult(
                                        maa::request::input::ToolCallResult {
                                            tool_call_id: "call_read".to_string(),
                                            result: Some(
                                                maa::request::input::tool_call_result::Result::ReadFiles(
                                                    maa::ReadFilesResult {
                                                        result: Some(
                                                            maa::read_files_result::Result::AnyFilesSuccess(
                                                                maa::read_files_result::AnyFilesSuccess {
                                                                    files: vec![maa::AnyFileContent {
                                                                        content: Some(
                                                                            maa::any_file_content::Content::TextContent(
                                                                                maa::FileContent {
                                                                                    file_path: "notes.md".to_string(),
                                                                                    content: "local notes".to_string(),
                                                                                    line_range: None,
                                                                                },
                                                                            ),
                                                                        ),
                                                                    }],
                                                                },
                                                            ),
                                                        ),
                                                    },
                                                ),
                                            ),
                                        },
                                    ),
                                ),
                            },
                            maa::request::input::user_inputs::UserInput {
                                input: Some(
                                    maa::request::input::user_inputs::user_input::Input::ToolCallResult(
                                        maa::request::input::ToolCallResult {
                                            tool_call_id: "call_bash".to_string(),
                                            result: Some(
                                                maa::request::input::tool_call_result::Result::RunShellCommand(
                                                    maa::RunShellCommandResult {
                                                        command: "ls".to_string(),
                                                        output: String::new(),
                                                        exit_code: 0,
                                                        result: Some(
                                                            maa::run_shell_command_result::Result::CommandFinished(
                                                                maa::ShellCommandFinished {
                                                                    command_id: "call_bash".to_string(),
                                                                    output: "Command: ls\nExit code: 0\n\nnotes.md\n".to_string(),
                                                                    exit_code: 0,
                                                                },
                                                            ),
                                                        ),
                                                    },
                                                ),
                                            ),
                                        },
                                    ),
                                ),
                            },
                        ],
                    },
                )),
            }),
            ..Default::default()
        };

        let model = test_openai_model("qwen");
        let messages = openai_messages_for_request(&request, &model);

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Let me check that.");
        assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_read");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call_bash");
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

    #[test]
    fn cloud_agent_initial_events_include_user_query_message() {
        let events = cloud_agent_initial_events("run-1", "WarpSOLO agent", "initial prompt");
        let messages = messages_from_response_events(&events);

        assert!(messages.iter().any(|message| matches!(
            message.message.as_ref(),
            Some(maa::message::Message::UserQuery(query)) if query.query == "initial prompt"
        )));
    }

    #[test]
    fn cloud_agent_followup_initial_events_include_user_query_message() {
        let events = cloud_agent_followup_initial_events("run-1", "follow-up prompt");
        let messages = messages_from_response_events(&events);

        assert!(messages.iter().any(|message| matches!(
            message.message.as_ref(),
            Some(maa::message::Message::UserQuery(query)) if query.query == "follow-up prompt"
        )));
    }

    #[test]
    fn agent_response_events_include_tool_messages() {
        let request = maa::Request::default();
        let run = LocalAgentRun {
            output: "done".to_string(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
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

    #[test]
    fn agent_response_events_include_reasoning_message() {
        let request = maa::Request::default();
        let run = LocalAgentRun {
            output: "done".to_string(),
            reasoning: "I should inspect the workspace first.".to_string(),
            tool_calls: Vec::new(),
            tool_events: Vec::new(),
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
            Some(maa::message::Message::AgentReasoning(_))
        ));
        assert!(matches!(
            messages[1].message,
            Some(maa::message::Message::AgentOutput(_))
        ));
    }
}
