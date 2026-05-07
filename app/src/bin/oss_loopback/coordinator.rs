use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use warp_multi_agent_api as maa;

use super::{
    add_messages_event, agent_output_message, agent_reasoning_message, bonjour, create_task_action,
    extract_user_prompt, finished_event, init_event, local_tool_call_message,
    local_tool_result_message, send_response_event, stream_ids, task_info, workspace_for_request,
    LocalToolCall, LocalToolEvent, LocalToolResult, ServerState,
};

const WORKER_DEFAULT_HARNESS: &str = "local-openai";

#[derive(Clone, Debug)]
pub(crate) struct RemoteWorkerRequest {
    worker_host: String,
    prompt: String,
    workspace: PathBuf,
    model_id: Option<String>,
    harness: Option<String>,
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
        state: String,
    },
}

pub(crate) async fn try_proxy_multi_agent_to_worker(
    state: ServerState,
    request: maa::Request,
    tx: mpsc::UnboundedSender<maa::ResponseEvent>,
) -> bool {
    let Some(remote_request) = remote_worker_request_from_maa(&request) else {
        return false;
    };
    let Some(worker) =
        bonjour::find_worker(&state.discovered_workers, &remote_request.worker_host).await
    else {
        return false;
    };

    tokio::spawn(async move {
        stream_remote_worker_run(state, request, remote_request, worker.url().to_string(), tx)
            .await;
    });
    true
}

async fn stream_remote_worker_run(
    state: ServerState,
    request: maa::Request,
    remote_request: RemoteWorkerRequest,
    worker_url: String,
    tx: mpsc::UnboundedSender<maa::ResponseEvent>,
) {
    let stream_ids = stream_ids(&request);
    let task_info = task_info(&request);
    send_response_event(&tx, init_event(&stream_ids));
    if task_info.needs_create {
        send_response_event(
            &tx,
            super::client_actions_event(vec![create_task_action(&task_info)]),
        );
    }

    let result = proxy_worker_events(
        &state,
        &worker_url,
        &remote_request,
        &tx,
        &task_info.id,
        &stream_ids.request_id,
    )
    .await;

    if let Err(err) = result {
        send_response_event(
            &tx,
            add_messages_event(
                &task_info.id,
                vec![agent_output_message(
                    &format!("Remote WarpSOLO worker failed: {err:#}"),
                    &task_info.id,
                    &stream_ids.request_id,
                )],
            ),
        );
    }
    send_response_event(&tx, finished_event());
}

async fn proxy_worker_events(
    state: &ServerState,
    worker_url: &str,
    remote_request: &RemoteWorkerRequest,
    tx: &mpsc::UnboundedSender<maa::ResponseEvent>,
    task_id: &str,
    request_id: &str,
) -> Result<()> {
    let create_url = join_worker_url(worker_url, "/worker/runs")?;
    let create = WorkerRunCreateRequest {
        prompt: remote_request.prompt.clone(),
        workspace: Some(remote_request.workspace.display().to_string()),
        model_id: remote_request.model_id.clone(),
        harness: remote_request.harness.clone(),
        source_device_id: state.account.device_id.clone(),
    };
    let mut request = state.client.post(create_url).json(&create);
    request = apply_worker_auth(request, state);
    let response = request
        .send()
        .await
        .context("failed to create remote worker run")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read remote worker run response")?;
    if !status.is_success() {
        anyhow::bail!("remote worker returned {status}: {body}");
    }
    let created: WorkerRunCreateResponse =
        serde_json::from_str(&body).context("failed to parse remote worker run response")?;
    log::info!(
        "Started WarpSOLO worker run {} on {}",
        created.run_id,
        worker_url
    );

    let events_url = join_worker_url(worker_url, &created.events_url)?;
    let mut request = state.client.get(events_url);
    request = apply_worker_auth(request, state);
    let response = request
        .send()
        .await
        .context("failed to connect to remote worker event stream")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("remote worker event stream returned {status}: {body}");
    }

    let mut tool_calls = HashMap::<String, LocalToolCall>::new();
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed to read remote worker event chunk")?;
        buffer.push_str(
            std::str::from_utf8(&chunk).context("remote worker event stream was not UTF-8")?,
        );
        while let Some(frame_end) = buffer.find("\n\n") {
            let frame = buffer[..frame_end].to_string();
            buffer.drain(..frame_end + 2);
            if let Some(event) = parse_worker_sse_frame(&frame)? {
                if relay_worker_event(event, tx, task_id, request_id, &mut tool_calls) {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

fn relay_worker_event(
    event: WorkerRunEvent,
    tx: &mpsc::UnboundedSender<maa::ResponseEvent>,
    task_id: &str,
    request_id: &str,
    tool_calls: &mut HashMap<String, LocalToolCall>,
) -> bool {
    match event {
        WorkerRunEvent::State { state } => {
            log::debug!("Remote WarpSOLO worker state changed to {state}");
        }
        WorkerRunEvent::ToolCall {
            id,
            name,
            arguments,
        } => {
            let tool_call = LocalToolCall {
                id: id.clone(),
                name,
                arguments,
            };
            tool_calls.insert(id, tool_call.clone());
            send_response_event(
                tx,
                add_messages_event(
                    task_id,
                    vec![local_tool_call_message(&tool_call, task_id, request_id)],
                ),
            );
        }
        WorkerRunEvent::ToolResult { id, name, summary } => {
            let tool_call = tool_calls.get(&id).cloned().unwrap_or(LocalToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: Value::Object(Default::default()),
            });
            let event = LocalToolEvent {
                tool_call,
                result: LocalToolResult {
                    tool_call_id: id,
                    name,
                    content: summary,
                },
            };
            send_response_event(
                tx,
                add_messages_event(
                    task_id,
                    vec![local_tool_result_message(&event, task_id, request_id)],
                ),
            );
        }
        WorkerRunEvent::ReasoningDelta { text } => {
            if !text.trim().is_empty() {
                send_response_event(
                    tx,
                    add_messages_event(
                        task_id,
                        vec![agent_reasoning_message(&text, task_id, request_id)],
                    ),
                );
            }
        }
        WorkerRunEvent::OutputDelta { text } => {
            if !text.trim().is_empty() {
                send_response_event(
                    tx,
                    add_messages_event(
                        task_id,
                        vec![agent_output_message(&text, task_id, request_id)],
                    ),
                );
            }
        }
        WorkerRunEvent::Error { message } => {
            send_response_event(
                tx,
                add_messages_event(
                    task_id,
                    vec![agent_output_message(
                        &format!("Remote worker error: {message}"),
                        task_id,
                        request_id,
                    )],
                ),
            );
        }
        WorkerRunEvent::Finished { state } => {
            log::debug!("Remote WarpSOLO worker finished with state {state}");
            return true;
        }
    }
    false
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
        .with_context(|| format!("failed to parse remote worker event: {data}"))
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

#[allow(deprecated)]
pub(crate) fn remote_worker_request_from_maa(
    request: &maa::Request,
) -> Option<RemoteWorkerRequest> {
    let config = approved_remote_orchestration_config(request)?;
    let worker_host = remote_worker_host(config)?;
    let prompt = extract_user_prompt(request)
        .filter(|prompt| !prompt.trim().is_empty())
        .unwrap_or_else(|| "Continue the current Warp agent conversation.".to_string());
    let workspace = workspace_for_request(request);
    Some(RemoteWorkerRequest {
        worker_host,
        prompt,
        workspace,
        model_id: (!config.model_id.trim().is_empty()).then(|| config.model_id.trim().to_string()),
        harness: Some(WORKER_DEFAULT_HARNESS.to_string()),
    })
}

#[allow(deprecated)]
fn approved_remote_orchestration_config(
    request: &maa::Request,
) -> Option<&maa::OrchestrationConfig> {
    match config_update_selection(request) {
        Some(Some(config)) => Some(config),
        Some(None) => None,
        None => approved_config_snapshot(request),
    }
}

#[allow(deprecated)]
fn config_update_selection(request: &maa::Request) -> Option<Option<&maa::OrchestrationConfig>> {
    use maa::request::input::user_inputs::user_input::Input as UserInput;
    use maa::request::input::Type;

    let Type::UserInputs(inputs) = request.input.as_ref()?.r#type.as_ref()? else {
        return None;
    };
    inputs.inputs.iter().rev().find_map(|input| {
        let UserInput::OrchestrationConfigUpdate(update) = input.input.as_ref()? else {
            return None;
        };
        Some(
            orchestration_status_is_approved(update.status.as_ref())
                .then(|| update.config.as_ref())
                .flatten(),
        )
    })
}

fn approved_config_snapshot(request: &maa::Request) -> Option<&maa::OrchestrationConfig> {
    request
        .task_context
        .as_ref()?
        .tasks
        .iter()
        .rev()
        .flat_map(|task| task.messages.iter().rev())
        .find_map(|message| {
            let maa::message::Message::OrchestrationConfigSnapshot(snapshot) =
                message.message.as_ref()?
            else {
                return None;
            };
            orchestration_status_is_approved(snapshot.status.as_ref())
                .then_some(snapshot.config.as_ref()?)
        })
}

fn orchestration_status_is_approved(status: Option<&maa::OrchestrationStatus>) -> bool {
    matches!(
        status.and_then(|status| status.status.as_ref()),
        Some(maa::orchestration_status::Status::Approved(_))
    )
}

fn remote_worker_host(config: &maa::OrchestrationConfig) -> Option<String> {
    let Some(maa::orchestration_config::ExecutionMode::Remote(remote)) =
        config.execution_mode.as_ref()
    else {
        return None;
    };
    Some(remote.worker_host.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_remote_worker_request_from_approved_config_update() {
        let request = maa::Request {
            input: Some(maa::request::Input {
                context: Some(maa::InputContext {
                    directory: Some(maa::input_context::Directory {
                        pwd: "/tmp/warp-solo-test".to_string(),
                        home: "/tmp".to_string(),
                        pwd_file_symbols_indexed: false,
                    }),
                    ..Default::default()
                }),
                r#type: Some(maa::request::input::Type::UserInputs(
                    maa::request::input::UserInputs {
                        inputs: vec![
                            maa::request::input::user_inputs::UserInput {
                                input: Some(
                                    maa::request::input::user_inputs::user_input::Input::UserQuery(
                                        maa::request::input::UserQuery {
                                            query: "run the tests".to_string(),
                                            ..Default::default()
                                        },
                                    ),
                                ),
                            },
                            maa::request::input::user_inputs::UserInput {
                                input: Some(
                                    maa::request::input::user_inputs::user_input::Input::OrchestrationConfigUpdate(
                                        maa::OrchestrationConfigUpdate {
                                            plan_id: "plan-1".to_string(),
                                            config: Some(remote_config("local-device-devbox")),
                                            status: Some(approved_status()),
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

        let remote = remote_worker_request_from_maa(&request).expect("remote worker request");
        assert_eq!(remote.worker_host, "local-device-devbox");
        assert_eq!(remote.prompt, "run the tests");
        assert_eq!(remote.workspace, PathBuf::from("/tmp/warp-solo-test"));
        assert_eq!(remote.harness.as_deref(), Some(WORKER_DEFAULT_HARNESS));
    }

    #[test]
    fn extracts_remote_worker_request_from_approved_snapshot() {
        let request = maa::Request {
            task_context: Some(maa::request::TaskContext {
                tasks: vec![maa::Task {
                    id: "task-1".to_string(),
                    description: "task".to_string(),
                    messages: vec![maa::Message {
                        id: "message-1".to_string(),
                        message: Some(maa::message::Message::OrchestrationConfigSnapshot(
                            maa::OrchestrationConfigSnapshot {
                                plan_id: "plan-1".to_string(),
                                config: Some(remote_config("warp")),
                                status: Some(approved_status()),
                            },
                        )),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }),
            input: Some(maa::request::Input {
                r#type: Some(maa::request::input::Type::UserInputs(
                    maa::request::input::UserInputs {
                        inputs: vec![maa::request::input::user_inputs::UserInput {
                            input: Some(
                                maa::request::input::user_inputs::user_input::Input::UserQuery(
                                    maa::request::input::UserQuery {
                                        query: "continue".to_string(),
                                        ..Default::default()
                                    },
                                ),
                            ),
                        }],
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };

        let remote = remote_worker_request_from_maa(&request).expect("remote worker request");
        assert_eq!(remote.worker_host, "warp");
        assert_eq!(remote.prompt, "continue");
    }

    #[test]
    fn ignores_disapproved_remote_config_update() {
        let request = maa::Request {
            input: Some(maa::request::Input {
                r#type: Some(maa::request::input::Type::UserInputs(
                    maa::request::input::UserInputs {
                        inputs: vec![maa::request::input::user_inputs::UserInput {
                            input: Some(
                                maa::request::input::user_inputs::user_input::Input::OrchestrationConfigUpdate(
                                    maa::OrchestrationConfigUpdate {
                                        plan_id: "plan-1".to_string(),
                                        config: Some(remote_config("local-device-devbox")),
                                        status: Some(maa::OrchestrationStatus {
                                            status: Some(
                                                maa::orchestration_status::Status::Disapproved(
                                                    maa::orchestration_status::Disapproved {},
                                                ),
                                            ),
                                        }),
                                    },
                                ),
                            ),
                        }],
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(remote_worker_request_from_maa(&request).is_none());
    }

    #[test]
    fn disapproved_update_overrides_approved_snapshot() {
        let request = maa::Request {
            task_context: Some(maa::request::TaskContext {
                tasks: vec![maa::Task {
                    id: "task-1".to_string(),
                    description: "task".to_string(),
                    messages: vec![maa::Message {
                        id: "message-1".to_string(),
                        message: Some(maa::message::Message::OrchestrationConfigSnapshot(
                            maa::OrchestrationConfigSnapshot {
                                plan_id: "plan-1".to_string(),
                                config: Some(remote_config("warp")),
                                status: Some(approved_status()),
                            },
                        )),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }),
            input: Some(maa::request::Input {
                r#type: Some(maa::request::input::Type::UserInputs(
                    maa::request::input::UserInputs {
                        inputs: vec![maa::request::input::user_inputs::UserInput {
                            input: Some(
                                maa::request::input::user_inputs::user_input::Input::OrchestrationConfigUpdate(
                                    maa::OrchestrationConfigUpdate {
                                        plan_id: "plan-1".to_string(),
                                        config: Some(remote_config("warp")),
                                        status: Some(maa::OrchestrationStatus {
                                            status: Some(
                                                maa::orchestration_status::Status::Disapproved(
                                                    maa::orchestration_status::Disapproved {},
                                                ),
                                            ),
                                        }),
                                    },
                                ),
                            ),
                        }],
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(remote_worker_request_from_maa(&request).is_none());
    }

    #[test]
    fn parses_worker_sse_data_frame() {
        let event = parse_worker_sse_frame(
            "event: message\ndata: {\"type\":\"output_delta\",\"text\":\"done\"}\n",
        )
        .unwrap()
        .unwrap();
        assert!(matches!(event, WorkerRunEvent::OutputDelta { text } if text == "done"));
    }

    fn remote_config(worker_host: &str) -> maa::OrchestrationConfig {
        maa::OrchestrationConfig {
            model_id: "local".to_string(),
            harness: Some(maa::Harness {
                variant: Some(maa::harness::Variant::Oz(maa::harness::Oz {})),
            }),
            execution_mode: Some(maa::orchestration_config::ExecutionMode::Remote(
                maa::orchestration_config::Remote {
                    environment_id: String::new(),
                    worker_host: worker_host.to_string(),
                },
            )),
        }
    }

    fn approved_status() -> maa::OrchestrationStatus {
        maa::OrchestrationStatus {
            status: Some(maa::orchestration_status::Status::Approved(
                maa::orchestration_status::Approved {},
            )),
        }
    }
}
