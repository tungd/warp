use std::path::Path;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use genai::{
    Client, Headers, ModelIden, ServiceTarget,
    adapter::AdapterKind,
    chat::{
        ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent,
        ReasoningEffort, Tool, ToolCall, ToolResponse,
    },
    resolver::{AuthData, Endpoint},
};
use serde_json::Value;

use super::{
    ResolvedLocalLlm,
    agent_state::{
        LocalAgentRun, LocalAssistantTurn, LocalToolCall, LocalToolEvent, LocalToolResult,
    },
};

const MAX_TOOL_ITERATIONS: usize = 8;

pub(super) async fn run_once_with_progress<OnToolCall, OnToolResult>(
    model: &ResolvedLocalLlm,
    messages: Vec<Value>,
    _workspace: &Path,
    mut on_tool_call: OnToolCall,
    _on_tool_result: OnToolResult,
) -> Result<LocalAgentRun>
where
    OnToolCall: FnMut(&LocalToolCall) + Send,
    OnToolResult: FnMut(&LocalToolEvent) + Send,
{
    let turn = complete_assistant_turn(model, messages).await?;
    for tool_call in &turn.tool_calls {
        on_tool_call(tool_call);
    }

    Ok(LocalAgentRun {
        output: turn.content,
        reasoning: turn.reasoning,
        tool_calls: turn.tool_calls,
        tool_events: Vec::new(),
    })
}

pub(super) async fn run_autonomous_with_progress<OnToolCall, OnToolResult>(
    model: &ResolvedLocalLlm,
    mut messages: Vec<Value>,
    workspace: &Path,
    mut on_tool_call: OnToolCall,
    mut on_tool_result: OnToolResult,
) -> Result<LocalAgentRun>
where
    OnToolCall: FnMut(&LocalToolCall) + Send,
    OnToolResult: FnMut(&LocalToolEvent) + Send,
{
    let mut output_parts = Vec::new();
    let mut reasoning_parts = Vec::new();
    let mut tool_events = Vec::new();

    for _ in 0..=MAX_TOOL_ITERATIONS {
        let turn = complete_assistant_turn(model, messages.clone()).await?;

        if !turn.reasoning.trim().is_empty() {
            reasoning_parts.push(turn.reasoning.clone());
        }
        if !turn.content.trim().is_empty() {
            output_parts.push(turn.content.clone());
        }

        if turn.tool_calls.is_empty() {
            return Ok(LocalAgentRun {
                output: output_parts.join("\n\n"),
                reasoning: reasoning_parts.join("\n\n"),
                tool_calls: Vec::new(),
                tool_events,
            });
        }

        messages.push(super::openai_assistant_message(&turn));
        for tool_call in turn.tool_calls {
            on_tool_call(&tool_call);
            let result = match super::execute_local_tool(&tool_call, workspace) {
                Ok(result) => result,
                Err(err) => LocalToolResult {
                    tool_call_id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    content: format!("Tool failed: {err:#}"),
                },
            };
            messages.push(super::openai_tool_result_message(&result));
            let event = LocalToolEvent { tool_call, result };
            on_tool_result(&event);
            tool_events.push(event);
        }
    }

    output_parts.push(format!(
        "Stopped after {MAX_TOOL_ITERATIONS} tool iterations without a final answer."
    ));
    Ok(LocalAgentRun {
        output: output_parts.join("\n\n"),
        reasoning: reasoning_parts.join("\n\n"),
        tool_calls: Vec::new(),
        tool_events,
    })
}

async fn complete_assistant_turn(
    model: &ResolvedLocalLlm,
    messages: Vec<Value>,
) -> Result<LocalAssistantTurn> {
    let client = Client::default();
    let target = service_target(model);
    let request = chat_request_from_openai_messages(model, messages)?;
    let options = chat_options(model);
    let response = client
        .exec_chat_stream(target, request, Some(&options))
        .await
        .context("failed to call local LLM through genai provider")?;

    let mut stream = response.stream;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    while let Some(event) = stream.next().await {
        match event.context("local LLM stream failed")? {
            ChatStreamEvent::Start => {}
            ChatStreamEvent::Chunk(chunk) => content.push_str(&chunk.content),
            ChatStreamEvent::ReasoningChunk(chunk) => reasoning.push_str(&chunk.content),
            ChatStreamEvent::ThoughtSignatureChunk(_) => {}
            ChatStreamEvent::ToolCallChunk(chunk) => {
                tool_calls.push(local_tool_call_from_genai(chunk.tool_call)?);
            }
            ChatStreamEvent::End(end) => {
                if content.trim().is_empty() {
                    if let Some(texts) = end.captured_texts() {
                        content = texts.join("");
                    }
                }
                if reasoning.trim().is_empty() {
                    if let Some(captured_reasoning) = end.captured_reasoning_content.as_ref() {
                        reasoning = captured_reasoning.clone();
                    }
                }
                if tool_calls.is_empty() {
                    if let Some(captured_tool_calls) = end.captured_tool_calls() {
                        tool_calls = captured_tool_calls
                            .into_iter()
                            .cloned()
                            .map(local_tool_call_from_genai)
                            .collect::<Result<Vec<_>>>()?;
                    }
                }
            }
        }
    }

    Ok(LocalAssistantTurn {
        content,
        reasoning,
        tool_calls,
    })
}

fn service_target(model: &ResolvedLocalLlm) -> ServiceTarget {
    ServiceTarget {
        endpoint: Endpoint::from_owned(normalized_endpoint_url(model)),
        auth: AuthData::from_single(model.token.clone()),
        model: ModelIden::new(adapter_kind(model), model.base_model_name.clone()),
    }
}

fn adapter_kind(model: &ResolvedLocalLlm) -> AdapterKind {
    match model.api_style.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => AdapterKind::Anthropic,
        "google" | "gemini" => AdapterKind::Gemini,
        "groq" => AdapterKind::Groq,
        "ollama" => AdapterKind::Ollama,
        "openai-resp" | "openai_resp" | "responses" => AdapterKind::OpenAIResp,
        "xai" | "grok" => AdapterKind::Xai,
        _ => AdapterKind::OpenAI,
    }
}

fn normalized_endpoint_url(model: &ResolvedLocalLlm) -> String {
    let mut url = model.base_url.trim().trim_end_matches('/').to_string();
    for suffix in [
        "/chat/completions",
        "/messages",
        "/responses",
        "/api/chat",
        "/embeddings",
    ] {
        if url.ends_with(suffix) {
            let new_len = url.len() - suffix.len();
            url.truncate(new_len);
            break;
        }
    }
    if adapter_kind(model) == AdapterKind::Anthropic && !url.ends_with("/v1") {
        url.push_str("/v1");
    }
    url.push('/');
    url
}

fn chat_options(model: &ResolvedLocalLlm) -> ChatOptions {
    let mut options = ChatOptions::default()
        .with_capture_usage(true)
        .with_capture_content(true)
        .with_capture_reasoning_content(true)
        .with_capture_tool_calls(true)
        .with_normalize_reasoning_content(true);

    if !model.headers.is_empty() {
        options = options.with_extra_headers(Headers::from(model.headers.clone()));
    }

    if let Some(effort) = reasoning_effort_for_model(model) {
        options = options.with_reasoning_effort(effort);
    }

    options
}

fn reasoning_effort_for_model(model: &ResolvedLocalLlm) -> Option<ReasoningEffort> {
    if !super::thinking_enabled(&model.thinking) {
        return None;
    }
    if let Some(budget) = model.thinking_budget {
        return Some(ReasoningEffort::Budget(budget));
    }

    Some(match model.thinking.trim().to_ascii_lowercase().as_str() {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "max" => ReasoningEffort::Max,
        "xhigh" | "extra-high" | "extra_high" => ReasoningEffort::XHigh,
        _ => ReasoningEffort::High,
    })
}

fn chat_request_from_openai_messages(
    model: &ResolvedLocalLlm,
    messages: Vec<Value>,
) -> Result<ChatRequest> {
    let mut system_parts = Vec::new();
    let mut chat_messages = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        match role {
            "system" => {
                if let Some(content) = message.get("content").and_then(text_content) {
                    if !content.trim().is_empty() {
                        system_parts.push(content);
                    }
                }
            }
            "user" => {
                let content = message
                    .get("content")
                    .and_then(text_content)
                    .unwrap_or_default();
                chat_messages.push(ChatMessage::user(content));
            }
            "assistant" => {
                if let Some(message) = assistant_message_from_openai_value(&message)? {
                    chat_messages.push(message);
                }
            }
            "tool" => {
                let tool_call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .unwrap_or("local-tool-call");
                let content = message
                    .get("content")
                    .and_then(text_content)
                    .unwrap_or_default();
                chat_messages.push(ChatMessage::from(ToolResponse::new(tool_call_id, content)));
            }
            _ => {}
        }
    }

    if system_parts.is_empty() {
        system_parts.push(super::local_agent_system_prompt(model));
    }

    let mut request =
        ChatRequest::from_messages(chat_messages).with_system(system_parts.join("\n\n"));
    let tools = genai_tools(model)?;
    if !tools.is_empty() {
        request = request.with_tools(tools);
    }
    Ok(request)
}

fn assistant_message_from_openai_value(message: &Value) -> Result<Option<ChatMessage>> {
    let content = message
        .get("content")
        .and_then(text_content)
        .unwrap_or_default();
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|tool_calls| {
            tool_calls
                .iter()
                .enumerate()
                .map(genai_tool_call_from_openai)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();

    let mut parts = Vec::new();
    if !content.trim().is_empty() {
        parts.push(ContentPart::Text(content));
    }
    parts.extend(tool_calls.into_iter().map(ContentPart::ToolCall));

    if parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ChatMessage::assistant(MessageContent::from_parts(
            parts,
        ))))
    }
}

fn genai_tool_call_from_openai((index, tool_call): (usize, &Value)) -> Result<ToolCall> {
    let function = tool_call
        .get("function")
        .context("tool call did not contain function data")?;
    let call_id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("local-tool-call-{index}"));
    let fn_name = function
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .context("tool call function name is required")?
        .to_owned();
    let fn_arguments = match function.get("arguments") {
        Some(Value::String(arguments)) if !arguments.trim().is_empty() => {
            serde_json::from_str(arguments)
                .with_context(|| format!("tool call '{fn_name}' arguments were not valid JSON"))?
        }
        Some(arguments) => arguments.clone(),
        None => serde_json::json!({}),
    };

    Ok(ToolCall {
        call_id,
        fn_name,
        fn_arguments,
        thought_signatures: None,
    })
}

fn local_tool_call_from_genai(tool_call: ToolCall) -> Result<LocalToolCall> {
    Ok(LocalToolCall {
        id: tool_call.call_id,
        name: tool_call.fn_name,
        arguments: tool_call.fn_arguments,
    })
}

fn genai_tools(model: &ResolvedLocalLlm) -> Result<Vec<Tool>> {
    super::local_openai_tools(model)
        .into_iter()
        .map(|tool| {
            let function = tool
                .get("function")
                .context("local tool descriptor did not contain function metadata")?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .context("local tool descriptor did not contain a function name")?;
            let description = function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "type": "object" }));

            Ok(Tool::new(name)
                .with_description(description)
                .with_schema(parameters))
        })
        .collect()
}

fn text_content(value: &Value) -> Option<String> {
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
