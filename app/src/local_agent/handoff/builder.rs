//! Builder for SpawnAgentRequest that isolates local agent workspace handling.
//!
//! This builder pattern prevents direct modification of upstream SpawnAgentRequest
//! construction sites, reducing merge conflicts during rebases.

use crate::ai::agent::UserQueryMode;
use crate::server::server_api::ai::{
    AgentConfigSnapshot, AttachmentInput, InitialSnapshotToken, SpawnAgentRequest,
};

/// Builder for SpawnAgentRequest with local workspace support.
pub struct LocalSpawnRequestBuilder {
    prompt: String,
    mode: UserQueryMode,
    config: Option<AgentConfigSnapshot>,
    title: Option<String>,
    team: Option<bool>,
    agent_identity_uid: Option<String>,
    skill: Option<String>,
    attachments: Vec<AttachmentInput>,
    interactive: Option<bool>,
    parent_run_id: Option<String>,
    runtime_skills: Vec<String>,
    referenced_attachments: Vec<String>,
    conversation_id: Option<String>,
    workspace: Option<String>,
    initial_snapshot_token: Option<InitialSnapshotToken>,
}

impl LocalSpawnRequestBuilder {
    /// Create a new builder with default values.
    pub fn new(prompt: String, mode: UserQueryMode) -> Self {
        Self {
            prompt,
            mode,
            config: None,
            title: None,
            team: None,
            agent_identity_uid: None,
            skill: None,
            attachments: Vec::new(),
            interactive: None,
            parent_run_id: None,
            runtime_skills: Vec::new(),
            referenced_attachments: Vec::new(),
            conversation_id: None,
            workspace: None,
            initial_snapshot_token: None,
        }
    }

    /// Create a builder from a handoff context.
    pub fn from_handoff(
        prompt: String,
        attachments: Vec<AttachmentInput>,
        forked_conversation_id: String,
        initial_snapshot_token: Option<InitialSnapshotToken>,
    ) -> Self {
        Self {
            prompt,
            mode: UserQueryMode::Normal,
            config: None,
            title: None,
            team: None,
            agent_identity_uid: None,
            skill: None,
            attachments,
            interactive: None,
            parent_run_id: None,
            runtime_skills: Vec::new(),
            referenced_attachments: Vec::new(),
            conversation_id: Some(forked_conversation_id),
            workspace: None,
            initial_snapshot_token,
        }
    }

    pub fn with_config(mut self, config: AgentConfigSnapshot) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    pub fn with_workspace(mut self, path: String) -> Self {
        self.workspace = Some(path);
        self
    }

    pub fn with_agent_identity_uid(mut self, uid: String) -> Self {
        self.agent_identity_uid = Some(uid);
        self
    }

    pub fn with_skill(mut self, skill: String) -> Self {
        self.skill = Some(skill);
        self
    }

    pub fn with_team(mut self, team: bool) -> Self {
        self.team = Some(team);
        self
    }

    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = Some(interactive);
        self
    }

    pub fn with_parent_run_id(mut self, run_id: String) -> Self {
        self.parent_run_id = Some(run_id);
        self
    }

    pub fn with_runtime_skills(mut self, skills: Vec<String>) -> Self {
        self.runtime_skills = skills;
        self
    }

    pub fn with_referenced_attachments(mut self, attachments: Vec<String>) -> Self {
        self.referenced_attachments = attachments;
        self
    }

    pub fn with_conversation_id(mut self, id: String) -> Self {
        self.conversation_id = Some(id);
        self
    }

    pub fn with_initial_snapshot_token(mut self, token: InitialSnapshotToken) -> Self {
        self.initial_snapshot_token = Some(token);
        self
    }

    /// Build the SpawnAgentRequest.
    pub fn build(self) -> SpawnAgentRequest {
        SpawnAgentRequest {
            prompt: self.prompt,
            mode: self.mode,
            config: self.config,
            title: self.title,
            team: self.team,
            agent_identity_uid: self.agent_identity_uid,
            skill: self.skill,
            attachments: self.attachments,
            interactive: self.interactive,
            parent_run_id: self.parent_run_id,
            runtime_skills: self.runtime_skills,
            referenced_attachments: self.referenced_attachments,
            conversation_id: self.conversation_id,
            workspace: self.workspace,
            initial_snapshot_token: self.initial_snapshot_token,
        }
    }
}
