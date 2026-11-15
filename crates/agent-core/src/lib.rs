//! EffiBuddy agent runtime.
//!
//! This crate is UI- and provider-agnostic. It contains:
//!
//! - a durable transcript model ([`message`], [`session`]);
//! - a provider boundary with a DeepSeek/OpenAI-compatible adapter
//!   ([`provider`], [`providers`]);
//! - a tool contract, approval gates and registry ([`tool`]);
//! - the agent loop ([`harness`]) with step budgets, interrupts, retries,
//!   panic isolation and rolling context compaction ([`context`]).
//!
//! The intended layering is:
//!
//! ```text
//!   GPUI app  ──reads──▶  SharedSession  ◀──writes──  AgentHarness
//!       ▲                     │                         │
//!       └──── HarnessEvent ───┴──── ModelProvider ──────┤
//!                                                       │
//!                                                 ToolRegistry
//! ```

pub mod checkpoint;
pub mod context;
pub mod contract;
pub mod error;
pub mod harness;
pub mod ids;
pub mod message;
pub mod model_config;
pub mod network;
pub mod profile_files;
pub mod prompt;
pub mod provider;
pub mod providers;
pub mod session;
pub mod subagent;
pub mod tool;
pub mod workflow_spec;

// Stress lab (opt-in via the `stress` feature): RAM-backed workspaces,
// process metrics and the virtual `StressProvider`. Compiled out of default
// builds entirely.
#[cfg(feature = "stress")]
pub mod ramfs;
#[cfg(feature = "stress")]
pub mod sys_metrics;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use checkpoint::{
    ContentDigest, FileCheckpoint, FileCheckpointSink, SharedFileCheckpointSink, TurnCheckpoint,
};
pub use contract::{extract_json_value, validate_json};
pub use error::{AgentError, ProviderError, ToolError};
pub use harness::{AgentHandle, AgentHarness, HarnessConfig, HarnessEvent};
pub use message::{
    AgentMessageMeta, ContentBlock, FinishReason, ImageAttachment, Message, Role, TodoItem,
    TodoStatus, ToolCall, ToolResult, Usage,
};
pub use model_config::{
    ApiFormat, DEFAULT_MODEL_ID, DEFAULT_PROVIDER_ID, MODEL_CONFIG_SCHEMA_VERSION,
    ModelCapabilities, ModelConfig, ModelConfigFile, ModelSelection, ProviderConfig,
    ReasoningLevel, ResolvedModel, builtin_providers, list_models, standard_reasoning_levels,
};
pub use network::{
    AgentDescriptor, AgentGraphEdgeRecord, AgentGraphNodeRecord, AgentGraphRecord, AgentMessage,
    AgentMessageKind, AgentNodeKind, AgentSendOutcome, AgentVisibility, GraphPoint,
    default_graph_zoom, normalize_agent_name,
};
pub use profile_files::{
    ProfileDiagnostic, load_profile_dir, merge_profiles, merged_subagent_profiles,
    parse_profile_markdown,
};
pub use provider::{ChatMessage, ChatRequest, Completion, ModelProvider, ProviderEvent, ToolSpec};
pub use providers::{
    AnthropicConfig, AnthropicMessagesProvider, DemoProvider, OpenAiCompatConfig,
    OpenAiCompatProvider, OpenAiResponsesConfig, OpenAiResponsesProvider,
};
#[cfg(feature = "stress")]
pub use providers::{StressProvider, stress::user_prompt as stress_user_prompt};
#[cfg(feature = "stress")]
pub use ramfs::RamDir;
pub use session::{
    ChildRunRecord, CompactionRecord, RunOutcome, RunStatus, SessionState, SharedSession,
    lock_session, new_shared_session,
};
pub use subagent::{
    MAX_AGENT_DEPTH, ProfileSource, SubagentColor, SubagentLaunch, SubagentProfile, SubagentReport,
    SubagentRequest, SubagentSpawner, SubagentStatus, builtin_subagent_profiles, find_profile,
    normalize_profile_name,
};
#[cfg(feature = "stress")]
pub use sys_metrics::{SysMonitor, SysSample};
pub use tool::{
    ApprovalDecision, ApprovalGate, ApprovalPolicy, ApprovalRequest, AutoApprovalGate,
    BrokerApprovalGate, DenyApprovalGate, NestedToolInvoker, Tool, ToolContext, ToolEffect,
    ToolOutput, ToolRegistry,
};
pub use workflow_spec::{
    WorkflowEdge, WorkflowNodeSpec, WorkflowSpec, render_node_prompt, render_template,
};
