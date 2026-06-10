#[derive(Clone, PartialEq, Message)]
struct AgentClientMessage {
    #[prost(message, optional, tag = "1")]
    run_request: Option<AgentRunRequest>,
    #[prost(message, optional, tag = "2")]
    exec_client_message: Option<ExecClientMessage>,
    #[prost(message, optional, tag = "3")]
    kv_client_message: Option<KvClientMessage>,
    #[prost(message, optional, tag = "4")]
    conversation_action: Option<ConversationAction>,
    #[prost(message, optional, tag = "5")]
    exec_client_control_message: Option<ExecClientControlMessage>,
    #[prost(message, optional, tag = "7")]
    client_heartbeat: Option<ClientHeartbeat>,
}

#[derive(Clone, PartialEq, Message)]
struct BidiRequestId {
    #[prost(string, tag = "1")]
    request_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct BidiAppendRequest {
    #[prost(string, tag = "1")]
    data: String,
    #[prost(message, optional, tag = "2")]
    request_id: Option<BidiRequestId>,
    #[prost(int64, tag = "3")]
    append_seqno: i64,
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
    #[prost(message, optional, tag = "6")]
    mcp_file_system_options: Option<McpFileSystemOptions>,
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
struct McpTools {
    #[prost(message, repeated, tag = "1")]
    tools: Vec<McpToolDefinition>,
}

#[derive(Clone, PartialEq, Message)]
struct McpToolDefinition {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    description: String,
    #[prost(message, optional, tag = "3")]
    input_schema: Option<::prost_types::Value>,
    #[prost(string, tag = "4")]
    provider_identifier: String,
    #[prost(string, tag = "5")]
    tool_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct McpInstructions {
    #[prost(string, tag = "1")]
    server_name: String,
    #[prost(string, tag = "2")]
    instructions: String,
}

#[derive(Clone, PartialEq, Message)]
struct McpFileSystemOptions {
    #[prost(bool, optional, tag = "1")]
    enabled: Option<bool>,
    #[prost(string, optional, tag = "2")]
    workspace_project_dir: Option<String>,
    #[prost(message, repeated, tag = "3")]
    descriptors: Vec<McpDescriptor>,
}

#[derive(Clone, PartialEq, Message)]
struct McpDescriptor {
    #[prost(string, tag = "1")]
    server_name: String,
    #[prost(string, tag = "2")]
    server_identifier: String,
    #[prost(string, optional, tag = "3")]
    folder_path: Option<String>,
    #[prost(string, optional, tag = "4")]
    server_use_instructions: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ConversationAction {
    #[prost(message, optional, tag = "1")]
    user_message_action: Option<UserMessageAction>,
    #[prost(message, optional, tag = "2")]
    resume_action: Option<ResumeAction>,
}

#[derive(Clone, PartialEq, Message)]
struct ResumeAction {
    #[prost(message, optional, tag = "2")]
    request_context: Option<RequestContext>,
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
    kv_server_message: Option<KvServerMessage>,
    #[prost(message, optional, tag = "5")]
    exec_server_control_message: Option<EmptyMessage>,
    #[prost(message, optional, tag = "7")]
    interaction_query: Option<InteractionQuery>,
}

#[derive(Clone, PartialEq, Message)]
#[allow(clippy::struct_field_names)]
struct InteractionQuery {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(message, optional, tag = "3")]
    ask_question_interaction_query: Option<AskQuestionInteractionQuery>,
    #[prost(message, optional, tag = "9")]
    web_fetch_request_query: Option<WebFetchRequestQuery>,
}

#[derive(Clone, PartialEq, Message)]
struct AskQuestionInteractionQuery {
    #[prost(message, optional, tag = "1")]
    args: Option<AskQuestionArgs>,
    #[prost(string, tag = "2")]
    tool_call_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct AskQuestionArgs {
    #[prost(string, tag = "1")]
    title: String,
    #[prost(message, repeated, tag = "2")]
    questions: Vec<AskQuestionQuestion>,
    #[prost(bool, optional, tag = "5")]
    run_async: Option<bool>,
    #[prost(string, optional, tag = "6")]
    async_original_tool_call_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct AskQuestionQuestion {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    prompt: String,
    #[prost(message, repeated, tag = "3")]
    options: Vec<AskQuestionOption>,
    #[prost(bool, optional, tag = "4")]
    allow_multiple: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct AskQuestionOption {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    label: String,
}

#[derive(Clone, PartialEq, Message)]
struct WebFetchRequestQuery {
    #[prost(message, optional, tag = "1")]
    args: Option<WebFetchArgs>,
    #[prost(bool, optional, tag = "2")]
    skip_approval: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(string, tag = "15")]
    exec_id: String,
    #[prost(message, optional, tag = "2")]
    shell_args: Option<ShellArgs>,
    #[prost(message, optional, tag = "3")]
    write_args: Option<WriteArgs>,
    #[prost(message, optional, tag = "4")]
    delete_args: Option<DeleteArgs>,
    #[prost(message, optional, tag = "5")]
    grep_args: Option<GrepArgs>,
    #[prost(message, optional, tag = "7")]
    read_args: Option<ReadArgs>,
    #[prost(message, optional, tag = "8")]
    ls_args: Option<LsArgs>,
    #[prost(message, optional, tag = "9")]
    diagnostics_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "10")]
    request_context_args: Option<RequestContextArgs>,
    #[prost(message, optional, tag = "11")]
    mcp_args: Option<McpArgs>,
    #[prost(message, optional, tag = "14")]
    shell_stream_args: Option<ShellArgs>,
    #[prost(message, optional, tag = "16")]
    background_shell_spawn_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "17")]
    list_mcp_resources_exec_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "18")]
    read_mcp_resource_exec_args: Option<EmptyMessage>,
    #[prost(message, optional, tag = "20")]
    fetch_args: Option<WebFetchArgs>,
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
struct ShellArgs {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    working_directory: String,
    #[prost(uint32, optional, tag = "3")]
    timeout: Option<u32>,
    #[prost(string, tag = "4")]
    tool_call_id: String,
    #[prost(string, repeated, tag = "5")]
    simple_commands: Vec<String>,
    #[prost(bool, optional, tag = "12")]
    skip_approval: Option<bool>,
    #[prost(string, optional, tag = "15")]
    description: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct WriteArgs {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    file_text: String,
    #[prost(string, tag = "3")]
    tool_call_id: String,
    #[prost(bool, optional, tag = "4")]
    return_file_content_after_write: Option<bool>,
    #[prost(string, optional, tag = "6")]
    encoding_hint: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct DeleteArgs {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    tool_call_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct GrepArgs {
    #[prost(string, tag = "1")]
    pattern: String,
    #[prost(string, optional, tag = "2")]
    path: Option<String>,
    #[prost(string, optional, tag = "3")]
    glob: Option<String>,
    #[prost(string, optional, tag = "4")]
    output_mode: Option<String>,
    #[prost(int32, optional, tag = "5")]
    context_before: Option<i32>,
    #[prost(int32, optional, tag = "6")]
    context_after: Option<i32>,
    #[prost(int32, optional, tag = "7")]
    context: Option<i32>,
    #[prost(bool, optional, tag = "8")]
    case_insensitive: Option<bool>,
    #[prost(string, optional, tag = "9")]
    type_name: Option<String>,
    #[prost(int32, optional, tag = "10")]
    head_limit: Option<i32>,
    #[prost(bool, optional, tag = "11")]
    multiline: Option<bool>,
    #[prost(string, optional, tag = "12")]
    sort: Option<String>,
    #[prost(bool, optional, tag = "13")]
    sort_ascending: Option<bool>,
    #[prost(string, tag = "14")]
    tool_call_id: String,
    #[prost(int32, optional, tag = "16")]
    offset: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct ReadArgs {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    tool_call_id: String,
    #[prost(int32, optional, tag = "4")]
    offset: Option<i32>,
    #[prost(uint32, optional, tag = "5")]
    limit: Option<u32>,
    #[prost(string, optional, tag = "6")]
    encoding_hint: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LsArgs {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, repeated, tag = "2")]
    ignore: Vec<String>,
    #[prost(string, tag = "3")]
    tool_call_id: String,
    #[prost(uint32, optional, tag = "5")]
    timeout_ms: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct RequestContextArgs {
    #[prost(string, optional, tag = "2")]
    notes_session_id: Option<String>,
    #[prost(string, optional, tag = "3")]
    workspace_id: Option<String>,
    #[prost(string, optional, tag = "4")]
    read_only_pinned_tree_sha: Option<String>,
    #[prost(string, optional, tag = "5")]
    read_only_plugin_cache_root: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct WebFetchArgs {
    #[prost(string, tag = "1")]
    url: String,
    #[prost(string, tag = "2")]
    tool_call_id: String,
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
    #[prost(message, repeated, tag = "7")]
    mcp_tools: Vec<McpToolDefinition>,
    #[prost(message, repeated, tag = "14")]
    mcp_instructions: Vec<McpInstructions>,
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
    #[prost(string, optional, tag = "11")]
    workspace_project_dir: Option<String>,
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

#[derive(Clone, PartialEq, Message)]
struct McpArgs {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(map = "string, message", tag = "2")]
    args: std::collections::HashMap<String, ::prost_types::Value>,
    #[prost(string, tag = "3")]
    tool_call_id: String,
    #[prost(string, tag = "4")]
    provider_identifier: String,
    #[prost(string, tag = "5")]
    tool_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(message, optional, tag = "2")]
    shell_result: Option<ShellResult>,
    #[prost(message, optional, tag = "3")]
    write_result: Option<WriteResult>,
    #[prost(message, optional, tag = "4")]
    delete_result: Option<DeleteResult>,
    #[prost(message, optional, tag = "5")]
    grep_result: Option<GrepResult>,
    #[prost(message, optional, tag = "7")]
    read_result: Option<ReadResult>,
    #[prost(message, optional, tag = "29")]
    redacted_read_result: Option<ReadResult>,
    #[prost(message, optional, tag = "8")]
    ls_result: Option<LsResult>,
    #[prost(message, optional, tag = "9")]
    diagnostics_result: Option<DiagnosticsResult>,
    #[prost(message, optional, tag = "10")]
    request_context_result: Option<RequestContextExecResult>,
    #[prost(message, optional, tag = "11")]
    mcp_result: Option<McpResult>,
    #[prost(message, optional, tag = "14")]
    shell_stream: Option<ShellStream>,
    #[prost(message, optional, tag = "20")]
    fetch_result: Option<FetchResult>,
    #[prost(string, optional, tag = "15")]
    exec_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ShellResult {
    #[prost(message, optional, tag = "4")]
    rejected: Option<ShellRejected>,
}

#[derive(Clone, PartialEq, Message)]
struct ShellRejected {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    working_directory: String,
    #[prost(string, tag = "3")]
    reason: String,
    #[prost(bool, tag = "4")]
    is_readonly: bool,
}

#[derive(Clone, PartialEq, Message)]
struct ShellStream {
    #[prost(message, optional, tag = "5")]
    rejected: Option<ShellRejected>,
}

#[derive(Clone, PartialEq, Message)]
struct GrepResult {
    #[prost(message, optional, tag = "2")]
    error: Option<GrepError>,
}

#[derive(Clone, PartialEq, Message)]
struct GrepError {
    #[prost(string, tag = "1")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct ReadResult {
    #[prost(message, optional, tag = "2")]
    error: Option<ReadError>,
}

#[derive(Clone, PartialEq, Message)]
struct ReadError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct LsResult {
    #[prost(message, optional, tag = "2")]
    error: Option<LsError>,
}

#[derive(Clone, PartialEq, Message)]
struct LsError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct WriteResult {
    #[prost(message, optional, tag = "5")]
    error: Option<WriteError>,
}

#[derive(Clone, PartialEq, Message)]
struct WriteError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct DeleteResult {
    #[prost(message, optional, tag = "7")]
    error: Option<DeleteError>,
}

#[derive(Clone, PartialEq, Message)]
struct DeleteError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct DiagnosticsResult {
    #[prost(message, optional, tag = "2")]
    error: Option<DiagnosticsError>,
}

#[derive(Clone, PartialEq, Message)]
struct DiagnosticsError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct FetchResult {
    #[prost(message, optional, tag = "2")]
    error: Option<FetchError>,
}

#[derive(Clone, PartialEq, Message)]
struct FetchError {
    #[prost(string, tag = "1")]
    url: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct ExecClientControlMessage {
    #[prost(message, optional, tag = "1")]
    stream_close: Option<ExecClientStreamClose>,
}

#[derive(Clone, PartialEq, Message)]
struct ExecClientStreamClose {
    #[prost(uint32, tag = "1")]
    id: u32,
}

#[derive(Clone, PartialEq, Message)]
struct McpResult {
    #[prost(message, optional, tag = "1")]
    success: Option<McpSuccess>,
    #[prost(message, optional, tag = "2")]
    error: Option<McpError>,
}

#[derive(Clone, PartialEq, Message)]
struct McpError {
    #[prost(string, tag = "1")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct McpSuccess {
    #[prost(message, repeated, tag = "1")]
    content: Vec<McpToolResultContentItem>,
    #[prost(bool, optional, tag = "2")]
    is_error: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct McpToolResultContentItem {
    #[prost(message, optional, tag = "1")]
    text: Option<McpTextContent>,
}

#[derive(Clone, PartialEq, Message)]
struct McpTextContent {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(message, optional, tag = "2")]
    get_blob_args: Option<KvGetBlobArgs>,
    #[prost(message, optional, tag = "3")]
    set_blob_args: Option<KvSetBlobArgs>,
}

#[derive(Clone, PartialEq, Message)]
struct KvGetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    blob_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct KvSetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    blob_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    blob_data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(message, optional, tag = "2")]
    get_blob_result: Option<KvGetBlobResult>,
    #[prost(message, optional, tag = "3")]
    set_blob_result: Option<KvSetBlobResult>,
}

#[derive(Clone, PartialEq, Message)]
struct KvGetBlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    data: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct KvSetBlobResult {}

#[derive(Clone, PartialEq, Message)]
struct RequestContextExecResult {
    #[prost(message, optional, tag = "1")]
    success: Option<RequestContextSuccess>,
}

#[derive(Clone, PartialEq, Message)]
struct RequestContextSuccess {
    #[prost(message, optional, tag = "1")]
    request_context: Option<RequestContext>,
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
            workspace_project_dir: cwd.clone(),
            sandbox_supported: Some(false),
            secret_redaction_enabled: Some(false),
            computer_use_supported: Some(false),
            is_working_dir_home_dir: Some(false),
            process_working_directory: cwd,
        }),
        mcp_tools: Vec::new(),
        mcp_instructions: Vec::new(),
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
