use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalAssistantTurn {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    pub(crate) tool_calls: Vec<LocalToolCall>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalToolResult {
    pub(crate) tool_call_id: String,
    pub(crate) name: String,
    pub(crate) content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalToolEvent {
    pub(crate) tool_call: LocalToolCall,
    pub(crate) result: LocalToolResult,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalAgentRun {
    pub(crate) output: String,
    pub(crate) reasoning: String,
    pub(crate) tool_calls: Vec<LocalToolCall>,
    pub(crate) tool_events: Vec<LocalToolEvent>,
}

impl LocalAgentRun {
    pub(crate) fn from_output(output: String) -> Self {
        Self {
            output,
            reasoning: String::new(),
            tool_calls: Vec::new(),
            tool_events: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct LocalCommandOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) timed_out: bool,
}
