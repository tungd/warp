use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use super::{bonjour, ServerState};

pub(crate) type CloudAgentRunStore = Arc<RwLock<HashMap<String, CloudAgentRunRecord>>>;

pub(crate) fn new_cloud_agent_run_store() -> CloudAgentRunStore {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Clone, Debug)]
pub(crate) struct CloudAgentRunRecord {
    run_id: String,
    worker_url: String,
    worker_run_id: String,
    worker_host: String,
    prompt: String,
    title: String,
    workspace: PathBuf,
    config: Option<Value>,
    state: CloudAgentRunState,
    status_message: Option<String>,
    final_output: Option<String>,
    created_at: String,
    started_at: Option<String>,
    updated_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloudAgentRunState {
    Pending,
    InProgress,
    Succeeded,
    Failed,
    Cancelled,
}

impl CloudAgentRunState {
    fn from_worker_state(state: &str) -> Option<Self> {
        match state {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::InProgress),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    fn as_api_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::InProgress => "InProgress",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpawnAgentRequest {
    prompt: String,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct RunFollowupRequest {
    message: String,
}

#[derive(Deserialize)]
pub(crate) struct ListRunsQuery {
    limit: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkerRunCreateRequest {
    prompt: String,
    workspace: Option<String>,
    model_id: Option<String>,
    harness: Option<String>,
    source_device_id: String,
}

#[derive(Serialize)]
struct WorkerRunFollowupRequest {
    prompt: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkerRunCreateResponse {
    run_id: String,
    events_url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerRunEvent {
    State {
        state: String,
    },
    ToolCall {
        id: String,
        name: String,
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
        state: String,
    },
}

pub(crate) async fn spawn_agent(
    State(state): State<ServerState>,
    Json(request): Json<SpawnAgentRequest>,
) -> Response {
    match spawn_agent_run(state, request).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

pub(crate) async fn get_agent_run(
    State(state): State<ServerState>,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    let runs = state.cloud_agent_runs.read().await;
    let Some(run) = runs.get(&run_id) else {
        return json_error(StatusCode::NOT_FOUND, "agent run was not found");
    };
    Json(agent_run_json(run)).into_response()
}

pub(crate) async fn list_agent_runs(
    State(state): State<ServerState>,
    Query(query): Query<ListRunsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(100);
    let mut runs = state
        .cloud_agent_runs
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    runs.truncate(limit);
    Json(
        runs.iter()
            .map(agent_run_json)
            .collect::<Vec<serde_json::Value>>(),
    )
    .into_response()
}

pub(crate) async fn submit_agent_followup(
    State(state): State<ServerState>,
    AxumPath(run_id): AxumPath<String>,
    Json(request): Json<RunFollowupRequest>,
) -> Response {
    let prompt = request.message.trim().to_string();
    if prompt.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "message is required");
    }

    let (worker_url, worker_run_id) = {
        let runs = state.cloud_agent_runs.read().await;
        let Some(run) = runs.get(&run_id) else {
            return json_error(StatusCode::NOT_FOUND, "agent run was not found");
        };
        if !run.state.is_terminal() {
            return json_error(
                StatusCode::CONFLICT,
                "cannot follow up while the agent run is still active",
            );
        }
        (run.worker_url.clone(), run.worker_run_id.clone())
    };

    match submit_worker_followup(&state, &worker_url, &worker_run_id, &prompt).await {
        Ok(created) => {
            reset_agent_run_for_followup(
                &state.cloud_agent_runs,
                &run_id,
                created.run_id.clone(),
                prompt,
            )
            .await;
            spawn_worker_event_pump(
                state,
                run_id,
                worker_url,
                created.run_id,
                created.events_url,
            );
            StatusCode::OK.into_response()
        }
        Err(err) => json_error(
            StatusCode::BAD_GATEWAY,
            &format!("failed to submit worker follow-up: {err:#}"),
        ),
    }
}

pub(crate) async fn cancel_agent_run(
    State(state): State<ServerState>,
    AxumPath(run_id): AxumPath<String>,
) -> Response {
    let (worker_url, worker_run_id) = {
        let runs = state.cloud_agent_runs.read().await;
        let Some(run) = runs.get(&run_id) else {
            return json_error(StatusCode::NOT_FOUND, "agent run was not found");
        };
        (run.worker_url.clone(), run.worker_run_id.clone())
    };

    let cancel_url =
        match join_worker_url(&worker_url, &format!("/worker/runs/{worker_run_id}/cancel")) {
            Ok(url) => url,
            Err(err) => return json_error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        };
    let mut request = state.client.post(cancel_url).json(&json!({}));
    request = apply_worker_auth(request, &state);
    match request.send().await {
        Ok(response) if response.status().is_success() => {
            update_agent_run_state(
                &state.cloud_agent_runs,
                &run_id,
                CloudAgentRunState::Cancelled,
                Some("Cancelled".to_string()),
            )
            .await;
            Json("cancelled").into_response()
        }
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            json_error(
                StatusCode::BAD_GATEWAY,
                &format!("worker cancel returned {status}: {body}"),
            )
        }
        Err(err) => json_error(
            StatusCode::BAD_GATEWAY,
            &format!("failed to cancel worker run: {err:#}"),
        ),
    }
}

async fn spawn_agent_run(
    state: ServerState,
    mut request: SpawnAgentRequest,
) -> std::result::Result<Value, Response> {
    request.prompt = request.prompt.trim().to_string();
    if request.prompt.is_empty() {
        return Err(json_error(StatusCode::BAD_REQUEST, "prompt is required"));
    }

    let worker_host = worker_host_from_config(request.config.as_ref());
    let Some(worker) = bonjour::find_worker(&state.discovered_workers, &worker_host).await else {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("no WarpSOLO worker is available for host '{worker_host}'"),
        ));
    };
    let workspace = workspace_from_spawn_request(&request)?;
    let worker_url = worker.url().to_string();
    let created = create_worker_run(&state, &worker_url, &request, &workspace).await?;
    let run_id = created.run_id.clone();
    let now = now_rfc3339();
    let record = CloudAgentRunRecord {
        run_id: run_id.clone(),
        worker_url: worker_url.clone(),
        worker_run_id: created.run_id.clone(),
        worker_host,
        prompt: request.prompt,
        title: request
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "WarpSOLO agent".to_string()),
        workspace,
        config: request.config,
        state: CloudAgentRunState::Pending,
        status_message: Some("Queued on WarpSOLO worker".to_string()),
        final_output: None,
        created_at: now.clone(),
        started_at: None,
        updated_at: now,
    };
    state
        .cloud_agent_runs
        .write()
        .await
        .insert(run_id.clone(), record);

    spawn_worker_event_pump(
        state,
        run_id.clone(),
        worker_url,
        created.run_id,
        created.events_url,
    );

    Ok(json!({
        "task_id": run_id,
        "run_id": run_id,
        "at_capacity": false,
    }))
}

async fn create_worker_run(
    state: &ServerState,
    worker_url: &str,
    request: &SpawnAgentRequest,
    workspace: &std::path::Path,
) -> std::result::Result<WorkerRunCreateResponse, Response> {
    let create_url = join_worker_url(worker_url, "/worker/runs").map_err(|err| {
        json_error(
            StatusCode::BAD_REQUEST,
            &format!("invalid worker URL: {err:#}"),
        )
    })?;
    let worker_request = WorkerRunCreateRequest {
        prompt: request.prompt.clone(),
        workspace: Some(workspace.display().to_string()),
        model_id: model_id_from_config(request.config.as_ref()),
        harness: Some("local-openai".to_string()),
        source_device_id: state.account.device_id.clone(),
    };
    let mut http_request = state.client.post(create_url).json(&worker_request);
    http_request = apply_worker_auth(http_request, state);
    let response = http_request
        .send()
        .await
        .map_err(|err| json_error(StatusCode::BAD_GATEWAY, &format!("{err:#}")))?;
    parse_worker_create_response(response).await
}

async fn submit_worker_followup(
    state: &ServerState,
    worker_url: &str,
    worker_run_id: &str,
    prompt: &str,
) -> Result<WorkerRunCreateResponse> {
    let followup_url = join_worker_url(
        worker_url,
        &format!("/worker/runs/{worker_run_id}/followup"),
    )?;
    let mut request = state
        .client
        .post(followup_url)
        .json(&WorkerRunFollowupRequest {
            prompt: prompt.to_string(),
        });
    request = apply_worker_auth(request, state);
    let response = request
        .send()
        .await
        .context("failed to submit worker follow-up")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read worker follow-up response")?;
    if !status.is_success() {
        anyhow::bail!("worker follow-up returned {status}: {body}");
    }
    serde_json::from_str(&body).context("failed to parse worker follow-up response")
}

async fn parse_worker_create_response(
    response: reqwest::Response,
) -> std::result::Result<WorkerRunCreateResponse, Response> {
    let status = response.status();
    let body = response.text().await.map_err(|err| {
        json_error(
            StatusCode::BAD_GATEWAY,
            &format!("failed to read worker response: {err:#}"),
        )
    })?;
    if !status.is_success() {
        return Err(json_error(
            StatusCode::BAD_GATEWAY,
            &format!("worker returned {status}: {body}"),
        ));
    }
    serde_json::from_str(&body).map_err(|err| {
        json_error(
            StatusCode::BAD_GATEWAY,
            &format!("failed to parse worker response: {err:#}"),
        )
    })
}

fn spawn_worker_event_pump(
    state: ServerState,
    run_id: String,
    worker_url: String,
    worker_run_id: String,
    events_url: String,
) {
    tokio::spawn(async move {
        if let Err(err) = pump_worker_events(
            state.clone(),
            &run_id,
            &worker_url,
            &worker_run_id,
            &events_url,
        )
        .await
        {
            update_agent_run_state(
                &state.cloud_agent_runs,
                &run_id,
                CloudAgentRunState::Failed,
                Some(format!("Worker event stream failed: {err:#}")),
            )
            .await;
        }
    });
}

async fn pump_worker_events(
    state: ServerState,
    run_id: &str,
    worker_url: &str,
    worker_run_id: &str,
    events_url: &str,
) -> Result<()> {
    let events_url = join_worker_url(worker_url, events_url)?;
    let mut request = state.client.get(events_url);
    request = apply_worker_auth(request, &state);
    let response = request
        .send()
        .await
        .context("failed to connect to worker event stream")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("worker event stream for {worker_run_id} returned {status}: {body}");
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed to read worker event chunk")?;
        buffer.push_str(std::str::from_utf8(&chunk).context("worker event stream was not UTF-8")?);
        while let Some(frame_end) = buffer.find("\n\n") {
            let frame = buffer[..frame_end].to_string();
            buffer.drain(..frame_end + 2);
            if let Some(event) = parse_worker_sse_frame(&frame)? {
                let finished =
                    relay_worker_event_to_record(&state.cloud_agent_runs, run_id, event).await;
                if finished {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

async fn relay_worker_event_to_record(
    store: &CloudAgentRunStore,
    run_id: &str,
    event: WorkerRunEvent,
) -> bool {
    match event {
        WorkerRunEvent::State { state } => {
            if let Some(state) = CloudAgentRunState::from_worker_state(&state) {
                update_agent_run_state(store, run_id, state, status_message_for_state(state)).await;
            }
        }
        WorkerRunEvent::ToolCall { id, name } => {
            update_agent_run_status_message(store, run_id, format!("Running tool {name} ({id})"))
                .await;
        }
        WorkerRunEvent::ToolResult { id, name, summary } => {
            let summary = truncate_status_message(&summary);
            update_agent_run_status_message(
                store,
                run_id,
                format!("Finished tool {name} ({id}): {summary}"),
            )
            .await;
        }
        WorkerRunEvent::ReasoningDelta { text } => {
            if !text.trim().is_empty() {
                update_agent_run_status_message(store, run_id, truncate_status_message(&text))
                    .await;
            }
        }
        WorkerRunEvent::OutputDelta { text } => {
            update_agent_run_output(store, run_id, text).await;
        }
        WorkerRunEvent::Error { message } => {
            update_agent_run_state(store, run_id, CloudAgentRunState::Failed, Some(message)).await;
        }
        WorkerRunEvent::Finished { state } => {
            let state = CloudAgentRunState::from_worker_state(&state)
                .unwrap_or(CloudAgentRunState::Succeeded);
            update_agent_run_state(store, run_id, state, status_message_for_state(state)).await;
            return true;
        }
    }
    false
}

async fn update_agent_run_state(
    store: &CloudAgentRunStore,
    run_id: &str,
    state: CloudAgentRunState,
    status_message: Option<String>,
) {
    let mut runs = store.write().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };
    run.state = state;
    if state == CloudAgentRunState::InProgress && run.started_at.is_none() {
        run.started_at = Some(now_rfc3339());
    }
    if let Some(message) = status_message {
        run.status_message = Some(message);
    }
    run.updated_at = now_rfc3339();
}

async fn update_agent_run_status_message(
    store: &CloudAgentRunStore,
    run_id: &str,
    message: String,
) {
    let mut runs = store.write().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };
    run.status_message = Some(message);
    run.updated_at = now_rfc3339();
}

async fn update_agent_run_output(store: &CloudAgentRunStore, run_id: &str, output: String) {
    let mut runs = store.write().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };
    run.final_output = Some(output.clone());
    run.status_message = Some(truncate_status_message(&output));
    run.updated_at = now_rfc3339();
}

async fn reset_agent_run_for_followup(
    store: &CloudAgentRunStore,
    run_id: &str,
    worker_run_id: String,
    prompt: String,
) {
    let mut runs = store.write().await;
    let Some(run) = runs.get_mut(run_id) else {
        return;
    };
    run.worker_run_id = worker_run_id;
    run.prompt = prompt;
    run.state = CloudAgentRunState::Pending;
    run.status_message = Some("Queued follow-up on WarpSOLO worker".to_string());
    run.final_output = None;
    run.started_at = None;
    run.updated_at = now_rfc3339();
}

fn agent_run_json(run: &CloudAgentRunRecord) -> Value {
    json!({
        "task_id": run.run_id,
        "parent_run_id": null,
        "title": run.title,
        "state": run.state.as_api_str(),
        "prompt": run.prompt,
        "created_at": run.created_at,
        "started_at": run.started_at,
        "updated_at": run.updated_at,
        "status_message": run.status_message.as_ref().map(|message| json!({ "message": message })),
        "source": null,
        "session_id": null,
        "session_link": null,
        "creator": null,
        "conversation_id": null,
        "request_usage": null,
        "is_sandbox_running": false,
        "agent_config_snapshot": agent_config_snapshot_json(run),
        "artifacts": [],
        "last_event_sequence": 0,
        "children": [],
    })
}

fn agent_config_snapshot_json(run: &CloudAgentRunRecord) -> Value {
    let mut config = run.config.clone().unwrap_or_else(|| json!({}));
    if let Some(object) = config.as_object_mut() {
        object.insert("worker_host".to_string(), json!(run.worker_host));
        object.insert(
            "workspace".to_string(),
            json!(run.workspace.display().to_string()),
        );
    }
    config
}

fn worker_host_from_config(config: Option<&Value>) -> String {
    config
        .and_then(|config| config.get("worker_host"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .unwrap_or("warp")
        .to_string()
}

fn model_id_from_config(config: Option<&Value>) -> Option<String> {
    config
        .and_then(|config| config.get("model_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
}

fn workspace_from_spawn_request(
    request: &SpawnAgentRequest,
) -> std::result::Result<PathBuf, Response> {
    let workspace = request
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if !workspace.is_dir() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "workspace must be an existing directory",
        ));
    }
    Ok(workspace)
}

fn status_message_for_state(state: CloudAgentRunState) -> Option<String> {
    Some(
        match state {
            CloudAgentRunState::Pending => "Queued on WarpSOLO worker",
            CloudAgentRunState::InProgress => "Running on WarpSOLO worker",
            CloudAgentRunState::Succeeded => "Worker run completed",
            CloudAgentRunState::Failed => "Worker run failed",
            CloudAgentRunState::Cancelled => "Worker run cancelled",
        }
        .to_string(),
    )
}

fn truncate_status_message(message: &str) -> String {
    const MAX_STATUS_BYTES: usize = 500;
    if message.len() <= MAX_STATUS_BYTES {
        return message.to_string();
    }
    let mut end = MAX_STATUS_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

fn parse_worker_sse_frame(frame: &str) -> Result<Option<WorkerRunEvent>> {
    let data = frame
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&data)
        .map(Some)
        .with_context(|| format!("failed to parse worker event: {data}"))
}

fn apply_worker_auth(
    request: reqwest::RequestBuilder,
    state: &ServerState,
) -> reqwest::RequestBuilder {
    if let Some(token) = state
        .worker_config
        .pairing_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        request.bearer_auth(token)
    } else {
        request
    }
}

fn join_worker_url(worker_url: &str, path: &str) -> Result<String> {
    let base = worker_url.trim_end_matches('/');
    if path.starts_with("http://") || path.starts_with("https://") {
        return Ok(path.to_string());
    }
    Ok(format!("{base}/{}", path.trim_start_matches('/')))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_host_defaults_to_warp() {
        assert_eq!(worker_host_from_config(None), "warp");
        assert_eq!(worker_host_from_config(Some(&json!({}))), "warp");
    }

    #[test]
    fn worker_host_reads_config_value() {
        assert_eq!(
            worker_host_from_config(Some(&json!({ "worker_host": "local-device-devbox" }))),
            "local-device-devbox"
        );
    }

    #[test]
    fn api_task_state_uses_warp_variant_names() {
        let now = now_rfc3339();
        let run = CloudAgentRunRecord {
            run_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            worker_url: "http://127.0.0.1:9109".to_string(),
            worker_run_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            worker_host: "warp".to_string(),
            prompt: "hello".to_string(),
            title: "WarpSOLO agent".to_string(),
            workspace: PathBuf::from("/tmp"),
            config: None,
            state: CloudAgentRunState::InProgress,
            status_message: Some("Running".to_string()),
            final_output: None,
            created_at: now.clone(),
            started_at: Some(now.clone()),
            updated_at: now,
        };

        let value = agent_run_json(&run);
        assert_eq!(value["state"], "InProgress");
        assert_eq!(value["session_id"], Value::Null);
        assert_eq!(value["status_message"]["message"], "Running");
    }

    #[test]
    fn parses_worker_event_frame() {
        let event = parse_worker_sse_frame("data: {\"type\":\"state\",\"state\":\"running\"}\n\n")
            .unwrap()
            .unwrap();
        assert!(matches!(event, WorkerRunEvent::State { state } if state == "running"));
    }
}
