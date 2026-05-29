#[derive(Clone, PartialEq, Message)]
struct AgentClientMessage {
    #[prost(message, optional, tag = "1")]
    run_request: Option<AgentRunRequest>,
    #[prost(message, optional, tag = "7")]
    client_heartbeat: Option<ClientHeartbeat>,
}

#[derive(Clone, PartialEq, Message)]
struct ClientHeartbeat {}

#[derive(Clone, PartialEq, Message)]
struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    action: Option<ConversationAction>,
    #[prost(message, optional, tag = "3")]
    model_details: Option<ModelDetails>,
    #[prost(message, optional, tag = "9")]
    requested_model: Option<RequestedModel>,
    #[prost(message, optional, tag = "4")]
    mcp_tools: Option<McpTools>,
    #[prost(string, optional, tag = "5")]
    conversation_id: Option<String>,
    #[prost(bool, optional, tag = "12")]
    exclude_workspace_context: Option<bool>,
    #[prost(string, optional, tag = "16")]
    conversation_group_id: Option<String>,
    #[prost(bool, optional, tag = "19")]
    client_supports_inline_images: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ConversationStateStructure {
    #[prost(int32, optional, tag = "10")]
    mode: Option<i32>,
    #[prost(uint64, optional, tag = "26")]
    conversation_started_timestamp_ms: Option<u64>,
    #[prost(string, optional, tag = "27")]
    conversation_started_time_zone: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct McpTools {}

#[derive(Clone, PartialEq, Message)]
struct ConversationAction {
    #[prost(message, optional, tag = "1")]
    user_message_action: Option<UserMessageAction>,
}

#[derive(Clone, PartialEq, Message)]
struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    user_message: Option<UserMessage>,
    #[prost(message, optional, tag = "2")]
    request_context: Option<RequestContext>,
}

#[derive(Clone, PartialEq, Message)]
struct UserMessage {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "2")]
    message_id: String,
    #[prost(message, optional, tag = "3")]
    selected_context: Option<SelectedContext>,
    #[prost(int32, tag = "4")]
    mode: i32,
    #[prost(bytes = "vec", tag = "10")]
    conversation_state_blob_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct SelectedContext {}

#[derive(Clone, PartialEq, Message)]
struct ModelDetails {
    #[prost(string, tag = "1")]
    model_id: String,
    #[prost(string, tag = "3")]
    display_model_id: String,
    #[prost(string, tag = "4")]
    display_name: String,
    #[prost(string, tag = "5")]
    display_name_short: String,
    #[prost(string, repeated, tag = "6")]
    aliases: Vec<String>,
    #[prost(bool, optional, tag = "7")]
    max_mode: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct RequestedModel {
    #[prost(string, tag = "1")]
    model_id: String,
    #[prost(bool, tag = "2")]
    max_mode: bool,
    #[prost(bool, tag = "7")]
    built_in_model: bool,
    #[prost(bool, tag = "8")]
    is_variant_string_representation: bool,
}

#[derive(Clone, PartialEq, Message)]
struct AgentServerMessage {
    #[prost(message, optional, tag = "1")]
    interaction_update: Option<InteractionUpdate>,
    #[prost(message, optional, tag = "2")]
    exec_server_message: Option<ExecServerMessage>,
    #[prost(message, optional, tag = "3")]
    conversation_checkpoint_update: Option<EmptyMessage>,
    #[prost(message, optional, tag = "4")]
    kv_server_message: Option<EmptyMessage>,
    #[prost(message, optional, tag = "5")]
    exec_server_control_message: Option<EmptyMessage>,
    #[prost(message, optional, tag = "7")]
    interaction_query: Option<EmptyMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(string, tag = "15")]
    exec_id: String,
    #[prost(message, optional, tag = "2")]
    shell_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "3")]
    write_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "4")]
    delete_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "5")]
    grep_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "7")]
    read_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "8")]
    ls_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "9")]
    diagnostics_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "10")]
    request_context_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "11")]
    mcp_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "14")]
    shell_stream_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "16")]
    background_shell_spawn_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "17")]
    list_mcp_resources_exec_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "18")]
    read_mcp_resource_exec_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "20")]
    fetch_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "21")]
    record_screen_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "22")]
    computer_use_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "23")]
    write_shell_stdin_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "27")]
    execute_hook_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "28")]
    subagent_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "29")]
    redacted_read_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "30")]
    force_background_shell_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "31")]
    force_background_subagent_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "36")]
    mcp_state_exec_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "37")]
    subagent_await_args: Option<EmptyMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct InteractionUpdate {
    #[prost(message, optional, tag = "1")]
    text_delta: Option<TextDeltaUpdate>,
    #[prost(message, optional, tag = "2")]
    tool_call_started: Option<EmptyMessage>,
    #[prost(message, optional, tag = "3")]
    tool_call_completed: Option<EmptyMessage>,
    #[prost(message, optional, tag = "4")]
    thinking_delta: Option<ThinkingDeltaUpdate>,
    #[prost(message, optional, tag = "6")]
    user_message_appended: Option<EmptyMessage>,
    #[prost(message, optional, tag = "7")]
    partial_tool_call: Option<EmptyMessage>,
    #[prost(message, optional, tag = "8")]
    token_delta: Option<EmptyMessage>,
    #[prost(message, optional, tag = "9")]
    summary: Option<EmptyMessage>,
    #[prost(message, optional, tag = "10")]
    summary_started: Option<EmptyMessage>,
    #[prost(message, optional, tag = "11")]
    summary_completed: Option<EmptyMessage>,
    #[prost(message, optional, tag = "12")]
    shell_output_delta: Option<EmptyMessage>,
    #[prost(message, optional, tag = "13")]
    heartbeat: Option<EmptyMessage>,
    #[prost(message, optional, tag = "14")]
    turn_ended: Option<TurnEndedUpdate>,
    #[prost(message, optional, tag = "15")]
    tool_call_delta: Option<EmptyMessage>,
    #[prost(message, optional, tag = "16")]
    step_started: Option<EmptyMessage>,
    #[prost(message, optional, tag = "17")]
    step_completed: Option<EmptyMessage>,
    #[prost(message, optional, tag = "18")]
    prompt_suggestion: Option<EmptyMessage>,
    #[prost(message, optional, tag = "19")]
    post_request_prompt: Option<EmptyMessage>,
    #[prost(message, optional, tag = "20")]
    active_branch_change: Option<EmptyMessage>,
    #[prost(message, optional, tag = "21")]
    feedback_request: Option<EmptyMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct TextDeltaUpdate {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct ThinkingDeltaUpdate {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
#[allow(clippy::struct_field_names)]
struct TurnEndedUpdate {
    #[prost(int64, optional, tag = "1")]
    input_tokens: Option<i64>,
    #[prost(int64, optional, tag = "2")]
    output_tokens: Option<i64>,
    #[prost(int64, optional, tag = "3")]
    cache_read_tokens: Option<i64>,
    #[prost(int64, optional, tag = "4")]
    cache_write_tokens: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
struct EmptyMessage {}

#[derive(Clone, PartialEq, Message)]
struct RequestContext {
    #[prost(message, optional, tag = "4")]
    env: Option<RequestContextEnv>,
    #[prost(bool, optional, tag = "17")]
    web_search_enabled: Option<bool>,
    #[prost(bool, optional, tag = "19")]
    repository_info_should_query_prod: Option<bool>,
    #[prost(bool, optional, tag = "24")]
    web_fetch_enabled: Option<bool>,
    #[prost(bool, optional, tag = "32")]
    supports_mcp_auth: Option<bool>,
    #[prost(bool, optional, tag = "33")]
    git_repo_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "36")]
    mcp_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "39")]
    rules_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "40")]
    env_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "41")]
    repository_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "42")]
    custom_subagents_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "43")]
    agent_skills_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "44")]
    mcp_file_system_info_complete: Option<bool>,
    #[prost(bool, optional, tag = "45")]
    git_status_info_complete: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct RequestContextEnv {
    #[prost(string, tag = "1")]
    os_version: String,
    #[prost(string, repeated, tag = "2")]
    workspace_paths: Vec<String>,
    #[prost(string, tag = "3")]
    shell: String,
    #[prost(bool, tag = "5")]
    sandbox_enabled: bool,
    #[prost(string, tag = "10")]
    time_zone: String,
    #[prost(bool, optional, tag = "14")]
    sandbox_supported: Option<bool>,
    #[prost(bool, optional, tag = "18")]
    secret_redaction_enabled: Option<bool>,
    #[prost(bool, optional, tag = "19")]
    computer_use_supported: Option<bool>,
    #[prost(bool, optional, tag = "20")]
    is_working_dir_home_dir: Option<bool>,
    #[prost(string, optional, tag = "21")]
    process_working_directory: Option<String>,
}

fn minimal_request_context() -> RequestContext {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok());
    RequestContext {
        env: Some(RequestContextEnv {
            os_version: std::env::consts::OS.to_owned(),
            workspace_paths: cwd.iter().cloned().collect(),
            shell: std::env::var("SHELL").unwrap_or_default(),
            sandbox_enabled: false,
            time_zone: std::env::var("TZ").unwrap_or_default(),
            sandbox_supported: Some(false),
            secret_redaction_enabled: Some(false),
            computer_use_supported: Some(false),
            is_working_dir_home_dir: Some(false),
            process_working_directory: cwd,
        }),
        web_search_enabled: Some(false),
        repository_info_should_query_prod: Some(false),
        web_fetch_enabled: Some(false),
        supports_mcp_auth: Some(false),
        git_repo_info_complete: Some(true),
        mcp_info_complete: Some(true),
        rules_info_complete: Some(true),
        env_info_complete: Some(true),
        repository_info_complete: Some(true),
        custom_subagents_info_complete: Some(true),
        agent_skills_info_complete: Some(true),
        mcp_file_system_info_complete: Some(true),
        git_status_info_complete: Some(true),
    }
}
