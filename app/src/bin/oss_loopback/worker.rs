use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, RwLock},
    task::AbortHandle,
};
use uuid::Uuid;

use super::{
    generate_worker_agent_output_with_progress, local_agent_error_message,
    state::LocalAgentWorkerConfig, LocalToolCall, LocalToolEvent, ServerState,
};

pub(crate) type WorkerRunStore = Arc<RwLock<HashMap<String, WorkerRunRecord>>>;

pub(crate) fn new_worker_run_store() -> WorkerRunStore {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerRunCreateRequest {
    prompt: String,
    workspace: Option<String>,
    model_id: Option<String>,
    harness: Option<String>,
    source_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerRunFollowupRequest {
    prompt: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkerRunCreateResponse {
    run_id: String,
    state: WorkerRunStatus,
    events_url: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkerRunStatusResponse {
    run_id: String,
    state: WorkerRunStatus,
    prompt: String,
    workspace: String,
    model_id: Option<String>,
    harness: Option<String>,
    source_device_id: Option<String>,
    created_at_epoch_millis: u64,
    updated_at_epoch_millis: u64,
    final_output: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkerRunStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl WorkerRunStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerRunEvent {
    State {
        state: WorkerRunStatus,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: String,
        name: String,
        summary: String,
    },
    ReasoningDelta {
        text: String,
    },
    OutputDelta {
        text: String,
    },
    Error {
        message: String,
    },
    Finished {
        state: WorkerRunStatus,
    },
}

pub(crate) struct WorkerRunRecord {
    run_id: String,
    status: WorkerRunStatus,
    prompt: String,
    workspace: PathBuf,
    model_id: Option<String>,
    harness: Option<String>,
    source_device_id: Option<String>,
    created_at_epoch_millis: u64,
    updated_at_epoch_millis: u64,
    final_output: Option<String>,
    error: Option<String>,
    events: Vec<WorkerRunEvent>,
    subscribers: Vec<mpsc::UnboundedSender<WorkerRunEvent>>,
    abort_handle: Option<AbortHandle>,
}

impl WorkerRunRecord {
    fn new(run_id: String, request: WorkerRunCreateRequest, workspace: PathBuf) -> Self {
        let now = now_epoch_millis();
        let mut run = Self {
            run_id,
            status: WorkerRunStatus::Pending,
            prompt: request.prompt,
            workspace,
            model_id: request.model_id,
            harness: request.harness,
            source_device_id: request.source_device_id,
            created_at_epoch_millis: now,
            updated_at_epoch_millis: now,
            final_output: None,
            error: None,
            events: Vec::new(),
            subscribers: Vec::new(),
            abort_handle: None,
        };
        run.push_event(WorkerRunEvent::State {
            state: WorkerRunStatus::Pending,
        });
        run
    }

    fn status_response(&self) -> WorkerRunStatusResponse {
        WorkerRunStatusResponse {
            run_id: self.run_id.clone(),
            state: self.status,
            prompt: self.prompt.clone(),
            workspace: self.workspace.display().to_string(),
            model_id: self.model_id.clone(),
            harness: self.harness.clone(),
            source_device_id: self.source_device_id.clone(),
            created_at_epoch_millis: self.created_at_epoch_millis,
            updated_at_epoch_millis: self.updated_at_epoch_millis,
            final_output: self.final_output.clone(),
            error: self.error.clone(),
        }
    }

    fn push_event(&mut self, event: WorkerRunEvent) {
        self.updated_at_epoch_millis = now_epoch_millis();
        self.events.push(event.clone());
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

pub(crate) async fn create_worker_run(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<WorkerRunCreateRequest>,
) -> Response {
    if let Err(response) = authorize_worker_request(&headers, &state.worker_config) {
        return response;
    }

    match start_worker_run(state, request).await {
        Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Err(response) => response,
    }
}

pub(crate) async fn get_worker_run(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize_worker_request(&headers, &state.worker_config) {
        return response;
    }

    let runs = state.worker_runs.read().await;
    let Some(run) = runs.get(&run_id) else {
        return json_error(StatusCode::NOT_FOUND, "worker run was not found");
    };
    Json(run.status_response()).into_response()
}

pub(crate) async fn worker_run_events(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize_worker_request(&headers, &state.worker_config) {
        return response;
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let mut runs = state.worker_runs.write().await;
        let Some(run) = runs.get_mut(&run_id) else {
            return json_error(StatusCode::NOT_FOUND, "worker run was not found");
        };
        for event in run.events.clone() {
            let _ = tx.send(event);
        }
        if !run.status.is_terminal() {
            run.subscribers.push(tx);
        }
    }

    let stream = async_stream::stream! {
        while let Some(event) = rx.recv().await {
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(worker_event_sse_chunk(&event)));
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

pub(crate) async fn followup_worker_run(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(run_id): AxumPath<String>,
    Json(followup): Json<WorkerRunFollowupRequest>,
) -> Response {
    if let Err(response) = authorize_worker_request(&headers, &state.worker_config) {
        return response;
    }

    let request = {
        let runs = state.worker_runs.read().await;
        let Some(run) = runs.get(&run_id) else {
            return json_error(StatusCode::NOT_FOUND, "worker run was not found");
        };
        if !run.status.is_terminal() {
            return json_error(
                StatusCode::CONFLICT,
                "cannot follow up while the worker run is still active",
            );
        }
        WorkerRunCreateRequest {
            prompt: followup.prompt,
            workspace: Some(run.workspace.display().to_string()),
            model_id: run.model_id.clone(),
            harness: run.harness.clone(),
            source_device_id: run.source_device_id.clone(),
        }
    };

    match start_worker_run(state, request).await {
        Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Err(response) => response,
    }
}

pub(crate) async fn cancel_worker_run(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize_worker_request(&headers, &state.worker_config) {
        return response;
    }

    let mut runs = state.worker_runs.write().await;
    let Some(run) = runs.get_mut(&run_id) else {
        return json_error(StatusCode::NOT_FOUND, "worker run was not found");
    };
    if !run.status.is_terminal() {
        if let Some(abort_handle) = run.abort_handle.take() {
            abort_handle.abort();
        }
        run.status = WorkerRunStatus::Cancelled;
        run.push_event(WorkerRunEvent::State {
            state: WorkerRunStatus::Cancelled,
        });
        run.push_event(WorkerRunEvent::Finished {
            state: WorkerRunStatus::Cancelled,
        });
    }

    Json(run.status_response()).into_response()
}

async fn start_worker_run(
    state: ServerState,
    request: WorkerRunCreateRequest,
) -> std::result::Result<WorkerRunCreateResponse, Response> {
    let request = normalize_create_request(request)?;
    let workspace = workspace_path(&request, &state.worker_config)?;
    let run_id = Uuid::new_v4().to_string();
    let record = WorkerRunRecord::new(run_id.clone(), request.clone(), workspace.clone());

    {
        let mut runs = state.worker_runs.write().await;
        runs.insert(run_id.clone(), record);
    }

    let task_state = state.clone();
    let task_run_id = run_id.clone();
    let handle = tokio::spawn(async move {
        run_worker_task(task_state, task_run_id, request, workspace).await;
    });
    let abort_handle = handle.abort_handle();
    {
        let mut runs = state.worker_runs.write().await;
        if let Some(run) = runs.get_mut(&run_id) {
            run.abort_handle = Some(abort_handle);
        }
    }

    Ok(WorkerRunCreateResponse {
        run_id: run_id.clone(),
        state: WorkerRunStatus::Pending,
        events_url: format!("/worker/runs/{run_id}/events"),
    })
}

async fn run_worker_task(
    state: ServerState,
    run_id: String,
    request: WorkerRunCreateRequest,
    workspace: PathBuf,
) {
    set_run_state(&state.worker_runs, &run_id, WorkerRunStatus::Running).await;

    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let progress_store = state.worker_runs.clone();
    let progress_run_id = run_id.clone();
    let progress_pump = tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            append_run_event(&progress_store, &progress_run_id, event).await;
        }
    });

    let tool_call_tx = progress_tx.clone();
    let tool_result_tx = progress_tx.clone();
    let result = generate_worker_agent_output_with_progress(
        &state,
        &request.prompt,
        &workspace,
        move |tool_call| {
            let _ = tool_call_tx.send(worker_tool_call_event(tool_call));
        },
        move |event| {
            let _ = tool_result_tx.send(worker_tool_result_event(event));
        },
    )
    .await;
    drop(progress_tx);
    let _ = progress_pump.await;

    match result {
        Ok(run) => {
            if !run.reasoning.trim().is_empty() {
                append_run_event(
                    &state.worker_runs,
                    &run_id,
                    WorkerRunEvent::ReasoningDelta {
                        text: run.reasoning,
                    },
                )
                .await;
            }
            if !run.output.trim().is_empty() {
                append_run_event(
                    &state.worker_runs,
                    &run_id,
                    WorkerRunEvent::OutputDelta {
                        text: run.output.clone(),
                    },
                )
                .await;
            }
            finish_run(
                &state.worker_runs,
                &run_id,
                WorkerRunStatus::Succeeded,
                Some(run.output),
                None,
            )
            .await;
        }
        Err(err) => {
            let message = local_agent_error_message(err);
            append_run_event(
                &state.worker_runs,
                &run_id,
                WorkerRunEvent::Error {
                    message: message.clone(),
                },
            )
            .await;
            finish_run(
                &state.worker_runs,
                &run_id,
                WorkerRunStatus::Failed,
                None,
                Some(message),
            )
            .await;
        }
    }
}

async fn set_run_state(store: &WorkerRunStore, run_id: &str, status: WorkerRunStatus) {
    let mut runs = store.write().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };
    if run.status.is_terminal() {
        return;
    }
    run.status = status;
    run.push_event(WorkerRunEvent::State { state: status });
}

async fn append_run_event(store: &WorkerRunStore, run_id: &str, event: WorkerRunEvent) {
    let mut runs = store.write().await;
    if let Some(run) = runs.get_mut(run_id) {
        run.push_event(event);
    }
}

async fn finish_run(
    store: &WorkerRunStore,
    run_id: &str,
    status: WorkerRunStatus,
    final_output: Option<String>,
    error: Option<String>,
) {
    let mut runs = store.write().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };
    if run.status.is_terminal() {
        return;
    }
    run.status = status;
    run.final_output = final_output;
    run.error = error;
    run.abort_handle = None;
    run.push_event(WorkerRunEvent::State { state: status });
    run.push_event(WorkerRunEvent::Finished { state: status });
}

fn normalize_create_request(
    mut request: WorkerRunCreateRequest,
) -> std::result::Result<WorkerRunCreateRequest, Response> {
    request.prompt = request.prompt.trim().to_string();
    if request.prompt.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "prompt is required"));
    }

    request.workspace = request
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
        .map(ToOwned::to_owned);
    request.model_id = request
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|model_id| !model_id.is_empty())
        .map(ToOwned::to_owned);
    request.harness = request
        .harness
        .as_deref()
        .map(str::trim)
        .filter(|harness| !harness.is_empty())
        .map(ToOwned::to_owned);
    request.source_device_id = request
        .source_device_id
        .as_deref()
        .map(str::trim)
        .filter(|source_device_id| !source_device_id.is_empty())
        .map(ToOwned::to_owned);

    if request
        .harness
        .as_deref()
        .is_some_and(|harness| harness != "local-openai")
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "only the local-openai worker harness is supported",
        ));
    }

    Ok(request)
}

fn workspace_path(
    request: &WorkerRunCreateRequest,
    worker_config: &LocalAgentWorkerConfig,
) -> std::result::Result<PathBuf, Response> {
    let workspace = request
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
        .map(|workspace| Ok(PathBuf::from(workspace)))
        .unwrap_or_else(|| worker_config.default_workspace_path());
    let workspace = workspace.map_err(|err| {
        json_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid workspace: {err:#}"),
        )
    })?;
    if !workspace.is_dir() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "workspace must be an existing directory",
        ));
    }
    Ok(workspace)
}

fn worker_tool_call_event(tool_call: &LocalToolCall) -> WorkerRunEvent {
    WorkerRunEvent::ToolCall {
        id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        arguments: tool_call.arguments.clone(),
    }
}

fn worker_tool_result_event(event: &LocalToolEvent) -> WorkerRunEvent {
    WorkerRunEvent::ToolResult {
        id: event.result.tool_call_id.clone(),
        name: event.result.name.clone(),
        summary: summarize_tool_result(&event.result.content),
    }
}

fn summarize_tool_result(content: &str) -> String {
    const MAX_SUMMARY_BYTES: usize = 4_000;
    if content.len() <= MAX_SUMMARY_BYTES {
        return content.to_string();
    }

    let mut end = MAX_SUMMARY_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...\n[tool result truncated]", &content[..end])
}

fn worker_event_sse_chunk(event: &WorkerRunEvent) -> String {
    let data = serde_json::to_string(event)
        .unwrap_or_else(|err| json!({ "type": "error", "message": err.to_string() }).to_string());
    format!("data: {data}\n\n")
}

fn authorize_worker_request(
    headers: &HeaderMap,
    worker_config: &LocalAgentWorkerConfig,
) -> std::result::Result<(), Response> {
    let Some(expected) = worker_config
        .pairing_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return Ok(());
    };

    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let explicit = headers
        .get("x-warpsolo-pairing-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);

    if bearer == Some(expected) || explicit == Some(expected) {
        Ok(())
    } else {
        Err(json_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid worker pairing token",
        ))
    }
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": message
        })),
    )
        .into_response()
}

fn now_epoch_millis() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis().try_into().unwrap_or(u64::MAX)
}
