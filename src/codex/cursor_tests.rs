use super::*;
use serde_json::json;
use crate::config::{AuthStore, Provider};
use std::collections::HashMap;

#[test]
fn builds_text_prompt_from_responses_body() {
    let body = json!({
        "model": "cursor/claude-4-sonnet",
        "instructions": "Be concise.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]},
            {"type": "function_call_output", "call_id": "call_1", "output": "42"}
        ]
    });

    let request = CursorRequest::from_body(&body).unwrap();

    assert_eq!(request.model, "cursor/claude-4-sonnet");
    assert_eq!(
        request.prompt,
        "User:\nHello\n\nAssistant:\nHi\n\nTool result call_1:\n42"
    );
}

#[test]
fn preserves_cursor_tools_in_request_body() {
    let body = json!({
        "model": "cursor/auto",
        "input": [{"type": "message", "role": "user", "content": "Hello"}],
        "tool_choice": "required",
        "tools": [{"type": "function", "name": "lookup"}]
    });

    let request = CursorRequest::from_body(&body).unwrap();

    assert_eq!(request.model, "cursor/auto");
    assert!(request.has_client_tools);
    assert_eq!(request.client_tools.len(), 1);
    assert_eq!(request.client_tools[0].name, "lookup");
    assert!(request.prompt.contains("Hello"));
}

#[test]
fn defaults_cursor_requests_to_agent_mode() {
    assert_eq!(parse_cursor_agent_mode(None).unwrap(), CURSOR_AGENT_MODE_AGENT);
    assert_eq!(cursor_agent_mode_name(CURSOR_AGENT_MODE_AGENT), "agent");
    assert_eq!(cursor_agent_mode_name(CURSOR_AGENT_MODE_ASK), "ask");
}

#[test]
fn parses_cursor_agent_mode_names_and_numbers() {
    assert_eq!(
        parse_cursor_agent_mode(Some("agent")).unwrap(),
        CURSOR_AGENT_MODE_AGENT
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("ASK")).unwrap(),
        CURSOR_AGENT_MODE_ASK
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("plan")).unwrap(),
        CURSOR_AGENT_MODE_PLAN
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("debug")).unwrap(),
        CURSOR_AGENT_MODE_DEBUG
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("triage")).unwrap(),
        CURSOR_AGENT_MODE_TRIAGE
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("project")).unwrap(),
        CURSOR_AGENT_MODE_PROJECT
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("multi-task")).unwrap(),
        CURSOR_AGENT_MODE_MULTITASK
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("1")).unwrap(),
        CURSOR_AGENT_MODE_AGENT
    );
    assert_eq!(
        parse_cursor_agent_mode(Some("7")).unwrap(),
        CURSOR_AGENT_MODE_MULTITASK
    );
}

#[test]
fn rejects_invalid_cursor_agent_mode() {
    let error = parse_cursor_agent_mode(Some("not-a-real-mode")).unwrap_err();
    assert!(error.contains(ROTOM_CURSOR_AGENT_MODE_ENV));

    let error = parse_cursor_agent_mode(Some("9")).unwrap_err();
    assert!(error.contains("0-7"));
}

#[test]
fn rejects_multimodal_cursor_content() {
    let body = json!({
        "model": "cursor/auto",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_image", "image_url": "data:image/png;base64,AA=="}]
        }]
    });

    let error = CursorRequest::from_body(&body).unwrap_err();

    assert!(error.to_string().contains("multimodal inputs"));
}

#[test]
fn ignores_cursor_tool_progress_frames_without_treating_them_as_interaction() {
    let message = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            tool_call_started: Some(EmptyMessage {}),
            tool_call_completed: Some(EmptyMessage {}),
            tool_call_delta: Some(EmptyMessage {}),
            shell_output_delta: Some(EmptyMessage {}),
            ..Default::default()
        }),
        exec_server_message: None,
        conversation_checkpoint_update: None,
        kv_server_message: None,
        exec_server_control_message: None,
        interaction_query: None,
    };

    assert!(matches!(
        cursor_frame_action(&message, &cursor_test_stream_context()),
        CursorFrameAction::None
    ));
    assert_eq!(cursor_probe_signal(&message), None);
}

#[test]
fn rejects_cursor_exec_frames_as_unsupported_tools() {
    let message = AgentServerMessage {
        interaction_update: None,
        exec_server_message: Some(ExecServerMessage {
            id: 7,
            exec_id: "exec_123".to_owned(),
            shell_stream_args: Some(ShellArgs {
                command: "printf probe".to_owned(),
                working_directory: "/tmp".to_owned(),
                timeout: Some(30_000),
                tool_call_id: "call_abc".to_owned(),
                simple_commands: vec!["printf probe".to_owned()],
                skip_approval: Some(true),
                description: Some("probe shell".to_owned()),
            }),
            shell_args: None,
            write_args: None,
            delete_args: None,
            grep_args: None,
            read_args: None,
            ls_args: None,
            diagnostics_args: None,
            request_context_args: None,
            mcp_args: None,
            background_shell_spawn_args: None,
            list_mcp_resources_exec_args: None,
            read_mcp_resource_exec_args: None,
            fetch_args: None,
            record_screen_args: None,
            computer_use_args: None,
            write_shell_stdin_args: None,
            execute_hook_args: None,
            subagent_args: None,
            redacted_read_args: None,
            force_background_shell_args: None,
            force_background_subagent_args: None,
            mcp_state_exec_args: None,
            subagent_await_args: None,
        }),
        conversation_checkpoint_update: None,
        kv_server_message: None,
        exec_server_control_message: None,
        interaction_query: None,
    };

    assert!(matches!(
        cursor_frame_action(&message, &cursor_test_stream_context()),
        CursorFrameAction::UnsupportedTool("shell_stream_args")
    ));
}

#[test]
fn declines_builtin_grep_with_steering_reason() {
    let exec = ExecServerMessage {
        id: 9,
        exec_id: "exec_grep".to_owned(),
        grep_args: Some(GrepArgs {
            pattern: "beta".to_owned(),
            tool_call_id: "call_grep".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let message = cursor_builtin_exec_decline(&exec).expect("grep should be declined");
    let exec_client = message.exec_client_message.expect("exec client message");
    assert_eq!(exec_client.id, 9);
    assert_eq!(exec_client.exec_id.as_deref(), Some("exec_grep"));
    let error = exec_client
        .grep_result
        .expect("grep result")
        .error
        .expect("grep error");
    assert_eq!(error.error, CURSOR_BUILTIN_TOOL_DECLINE_REASON);
}

#[test]
fn declines_builtin_read_ls_write_delete_with_paths() {
    let read = cursor_builtin_exec_decline(&ExecServerMessage {
        read_args: Some(ReadArgs {
            path: "/tmp/x".to_owned(),
            tool_call_id: "c".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    })
    .and_then(|m| m.exec_client_message)
    .and_then(|m| m.read_result)
    .and_then(|r| r.error)
    .expect("read error");
    assert_eq!(read.path, "/tmp/x");
    assert_eq!(read.error, CURSOR_BUILTIN_TOOL_DECLINE_REASON);

    let ls = cursor_builtin_exec_decline(&ExecServerMessage {
        ls_args: Some(LsArgs {
            path: "/tmp".to_owned(),
            tool_call_id: "c".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    })
    .and_then(|m| m.exec_client_message)
    .and_then(|m| m.ls_result)
    .and_then(|r| r.error)
    .expect("ls error");
    assert_eq!(ls.path, "/tmp");

    let write = cursor_builtin_exec_decline(&ExecServerMessage {
        write_args: Some(WriteArgs {
            path: "/tmp/out".to_owned(),
            tool_call_id: "c".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    })
    .and_then(|m| m.exec_client_message)
    .and_then(|m| m.write_result)
    .and_then(|r| r.error)
    .expect("write error");
    assert_eq!(write.path, "/tmp/out");

    let delete = cursor_builtin_exec_decline(&ExecServerMessage {
        delete_args: Some(DeleteArgs {
            path: "/tmp/gone".to_owned(),
            tool_call_id: "c".to_owned(),
        }),
        ..Default::default()
    })
    .and_then(|m| m.exec_client_message)
    .and_then(|m| m.delete_result)
    .and_then(|r| r.error)
    .expect("delete error");
    assert_eq!(delete.path, "/tmp/gone");
}

#[test]
fn declines_builtin_shell_variants_and_encodes() {
    let shell = ExecServerMessage {
        id: 1,
        shell_args: Some(ShellArgs {
            command: "cat one.txt".to_owned(),
            working_directory: "/tmp".to_owned(),
            tool_call_id: "c".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let message = cursor_builtin_exec_decline(&shell).expect("shell declined");
    assert!(!message.encode_to_vec().is_empty());
    let rejected = message
        .exec_client_message
        .expect("ecm")
        .shell_result
        .expect("shell result")
        .rejected
        .expect("rejected");
    assert_eq!(rejected.command, "cat one.txt");
    assert_eq!(rejected.working_directory, "/tmp");
    assert_eq!(rejected.reason, CURSOR_BUILTIN_TOOL_DECLINE_REASON);

    let stream = ExecServerMessage {
        id: 2,
        shell_stream_args: Some(ShellArgs {
            command: "ls".to_owned(),
            tool_call_id: "c".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let reason = cursor_builtin_exec_decline(&stream)
        .and_then(|m| m.exec_client_message)
        .and_then(|m| m.shell_stream)
        .and_then(|s| s.rejected)
        .map(|r| r.reason)
        .expect("shell stream rejected");
    assert_eq!(reason, CURSOR_BUILTIN_TOOL_DECLINE_REASON);
}

#[test]
fn does_not_decline_bridged_mcp_tool_call() {
    let exec = ExecServerMessage {
        mcp_args: Some(McpArgs {
            name: "rotom-tools-BrewCoffee".to_owned(),
            args: HashMap::new(),
            tool_call_id: "c".to_owned(),
            provider_identifier: ROTOM_CURSOR_MCP_PROVIDER.to_owned(),
            tool_name: "BrewCoffee".to_owned(),
        }),
        ..Default::default()
    };
    assert!(cursor_builtin_exec_decline(&exec).is_none());
}

#[test]
fn builds_rotom_mcp_tool_metadata_from_client_tools() {
    let request = CursorRequest::from_body(&json!({
        "model": "cursor/auto",
        "input": [{"type": "message", "role": "user", "content": "Brew a latte"}],
        "tools": [{
            "type": "function",
            "name": "BrewCoffee",
            "description": "Brews coffee drinks.",
            "parameters": {
                "type": "object",
                "properties": {
                    "drink": {"type": "string"}
                },
                "required": ["drink"]
            }
        }]
    }))
    .unwrap();
    let model = CursorModel {
        model_id: "claude-4-sonnet".to_owned(),
        display_model_id: "claude-4-sonnet".to_owned(),
        display_name: "Sonnet 4".to_owned(),
        display_name_short: "Sonnet".to_owned(),
        aliases: vec![],
        max_mode: Some(false),
    };

    let message = build_agent_client_message_with_mode(
        &request,
        &model,
        "conv_1",
        "msg_1",
        CURSOR_AGENT_MODE_AGENT,
    );
    let run_request = message.run_request.expect("run request");
    let mcp_tools = run_request.mcp_tools.expect("mcp tools");
    let tool = mcp_tools.tools.first().expect("first mcp tool");
    let request_context = run_request
        .action
        .and_then(|action| action.user_message_action)
        .and_then(|action| action.request_context)
        .expect("request context");

    assert_eq!(tool.provider_identifier, ROTOM_CURSOR_MCP_PROVIDER);
    assert_eq!(tool.tool_name, "BrewCoffee");
    assert_eq!(tool.name, "rotom-tools-BrewCoffee");
    assert_eq!(request_context.mcp_tools.len(), 1);
    assert_eq!(request_context.mcp_instructions.len(), 1);
    assert!(run_request.mcp_file_system_options.is_some());
}

#[test]
fn converts_rotom_mcp_exec_frames_into_tool_calls() {
    let mut args = HashMap::new();
    args.insert(
        "drink".to_owned(),
        json_to_proto_value(&json!("latte")),
    );
    args.insert("size".to_owned(), json_to_proto_value(&json!("large")));
    let message = AgentServerMessage {
        interaction_update: None,
        exec_server_message: Some(ExecServerMessage {
            id: 9,
            exec_id: "exec_mcp_1".to_owned(),
            shell_args: None,
            write_args: None,
            delete_args: None,
            grep_args: None,
            read_args: None,
            ls_args: None,
            diagnostics_args: None,
            request_context_args: None,
            mcp_args: Some(McpArgs {
                name: "rotom-tools-BrewCoffee".to_owned(),
                args,
                tool_call_id: "call_brew_1".to_owned(),
                provider_identifier: ROTOM_CURSOR_MCP_PROVIDER.to_owned(),
                tool_name: "BrewCoffee".to_owned(),
            }),
            shell_stream_args: None,
            background_shell_spawn_args: None,
            list_mcp_resources_exec_args: None,
            read_mcp_resource_exec_args: None,
            fetch_args: None,
            record_screen_args: None,
            computer_use_args: None,
            write_shell_stdin_args: None,
            execute_hook_args: None,
            subagent_args: None,
            redacted_read_args: None,
            force_background_shell_args: None,
            force_background_subagent_args: None,
            mcp_state_exec_args: None,
            subagent_await_args: None,
        }),
        conversation_checkpoint_update: None,
        kv_server_message: None,
        exec_server_control_message: None,
        interaction_query: None,
    };

    match cursor_frame_action(&message, &cursor_test_stream_context()) {
        CursorFrameAction::ToolCall(tool_call, _) => {
            assert_eq!(tool_call.id, "call_brew_1");
            assert_eq!(tool_call.function.name, "BrewCoffee");
            assert_eq!(
                serde_json::from_str::<Value>(&tool_call.function.arguments).unwrap(),
                json!({"drink": "latte", "size": "large"})
            );
        }
        _ => panic!("expected a bridged tool call"),
    }
}

#[test]
fn rejects_ask_question_interaction_query_as_unsupported_tool() {
    let message = AgentServerMessage {
        interaction_update: None,
        exec_server_message: None,
        conversation_checkpoint_update: None,
        kv_server_message: None,
        exec_server_control_message: None,
        interaction_query: Some(InteractionQuery {
            id: 41,
            ask_question_interaction_query: Some(AskQuestionInteractionQuery {
                args: Some(AskQuestionArgs {
                    title: "Need input".to_owned(),
                    questions: vec![
                        AskQuestionQuestion {
                            id: "color".to_owned(),
                            prompt: "Pick a color".to_owned(),
                            options: vec![
                                AskQuestionOption {
                                    id: "red".to_owned(),
                                    label: "Red".to_owned(),
                                },
                                AskQuestionOption {
                                    id: "blue".to_owned(),
                                    label: "Blue".to_owned(),
                                },
                            ],
                            allow_multiple: Some(false),
                        },
                        AskQuestionQuestion {
                            id: "notes".to_owned(),
                            prompt: "Add notes".to_owned(),
                            options: Vec::new(),
                            allow_multiple: Some(true),
                        },
                    ],
                    run_async: Some(true),
                    async_original_tool_call_id: Some("orig_call".to_owned()),
                }),
                tool_call_id: "ask_call_1".to_owned(),
            }),
            web_fetch_request_query: None,
        }),
    };

    assert!(matches!(
        cursor_frame_action(&message, &cursor_test_stream_context()),
        CursorFrameAction::UnsupportedTool("ask_question_interaction_query")
    ));
}

#[test]
fn rejects_web_fetch_interaction_query_as_unsupported_tool() {
    let message = AgentServerMessage {
        interaction_update: None,
        exec_server_message: None,
        conversation_checkpoint_update: None,
        kv_server_message: None,
        exec_server_control_message: None,
        interaction_query: Some(InteractionQuery {
            id: 52,
            ask_question_interaction_query: None,
            web_fetch_request_query: Some(WebFetchRequestQuery {
                args: Some(WebFetchArgs {
                    url: "https://example.com".to_owned(),
                    tool_call_id: "fetch_call_1".to_owned(),
                }),
                skip_approval: Some(true),
            }),
        }),
    };

    assert!(matches!(
        cursor_frame_action(&message, &cursor_test_stream_context()),
        CursorFrameAction::UnsupportedTool("web_fetch_request_query")
    ));
}

#[test]
fn selects_alias_and_compatibility_models() {
    let models = vec![
        CursorModel {
            model_id: "default".to_owned(),
            display_model_id: "auto".to_owned(),
            display_name: "Auto".to_owned(),
            display_name_short: "Auto".to_owned(),
            aliases: vec!["auto".to_owned()],
            max_mode: Some(false),
        },
        CursorModel {
            model_id: "gpt-5.2".to_owned(),
            display_model_id: "gpt-5.2".to_owned(),
            display_name: "GPT 5.2".to_owned(),
            display_name_short: "GPT".to_owned(),
            aliases: vec!["gpt".to_owned()],
            max_mode: None,
        },
        CursorModel {
            model_id: "claude-4-sonnet".to_owned(),
            display_model_id: "claude-4-sonnet".to_owned(),
            display_name: "Sonnet 4".to_owned(),
            display_name_short: "Sonnet".to_owned(),
            aliases: vec![],
            max_mode: None,
        },
    ];

    assert_eq!(
        select_cursor_model("cursor/auto", &models)
            .unwrap()
            .model_id,
        "default"
    );
    assert_eq!(
        select_cursor_model("cursor/gpt-5", &models)
            .unwrap()
            .model_id,
        "gpt-5.2"
    );
    assert_eq!(
        select_cursor_model("cursor/sonnet-4", &models)
            .unwrap()
            .model_id,
        "claude-4-sonnet"
    );
}

#[test]
fn rejects_unknown_explicit_cursor_model() {
    let models = vec![CursorModel {
        model_id: "default".to_owned(),
        display_model_id: "auto".to_owned(),
        display_name: "Auto".to_owned(),
        display_name_short: "Auto".to_owned(),
        aliases: vec!["auto".to_owned()],
        max_mode: Some(false),
    }];

    let error = select_cursor_model("cursor/not-real", &models).unwrap_err();

    assert!(error.to_string().contains("does not support model"));
}

#[test]
fn formats_live_cursor_model_ids_with_compatibility_aliases() {
    let models = vec![
        CursorModel {
            model_id: "default".to_owned(),
            display_model_id: "auto".to_owned(),
            display_name: "Auto".to_owned(),
            display_name_short: "Auto".to_owned(),
            aliases: vec![],
            max_mode: Some(false),
        },
        CursorModel {
            model_id: "composer-2.5".to_owned(),
            display_model_id: "composer-2.5".to_owned(),
            display_name: "Composer".to_owned(),
            display_name_short: "Composer".to_owned(),
            aliases: vec![],
            max_mode: None,
        },
        CursorModel {
            model_id: "claude-4-sonnet".to_owned(),
            display_model_id: "claude-4-sonnet".to_owned(),
            display_name: "Sonnet 4".to_owned(),
            display_name_short: "Sonnet".to_owned(),
            aliases: vec![],
            max_mode: None,
        },
    ];

    let ids = cursor_model_ids(&models);

    assert_eq!(ids.first().map(String::as_str), Some("cursor/auto"));
    assert!(ids.iter().any(|id| id == "cursor/composer-2.5"));
    assert!(ids.iter().any(|id| id == "cursor/claude-4-sonnet"));
    assert!(ids.iter().any(|id| id == "cursor/gpt-5"));
    assert!(ids.iter().any(|id| id == "cursor/sonnet-4"));
    assert!(!ids.iter().any(|id| id == "cursor/default"));
}

#[test]
fn builds_connect_proto_envelope() {
    let body = connect_proto_envelope(&[1, 2, 3]);

    assert_eq!(body[0], 0);
    assert_eq!(
        u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize,
        body.len() - 5
    );
    assert_eq!(&body[5..], &[1, 2, 3]);
}

#[test]
fn decodes_cursor_text_delta_frames() {
    let message = AgentServerMessage {
        interaction_update: Some(InteractionUpdate {
            text_delta: Some(TextDeltaUpdate {
                text: "OK".to_owned(),
            }),
            ..Default::default()
        }),
        exec_server_message: None,
        conversation_checkpoint_update: None,
        kv_server_message: None,
        exec_server_control_message: None,
        interaction_query: None,
    };
    let encoded_message = message.encode_to_vec();
    let mut payload = connect_proto_envelope(&encoded_message);
    payload.extend_from_slice(&[CONNECT_DATA_FLAG_END_STREAM, 0, 0, 0, 0]);
    let mut decoder = ConnectFrameDecoder::default();
    let frames = decoder.push(&payload).unwrap();

    assert_eq!(frames.len(), 2);
    match &frames[0] {
        ConnectFrame::Data(value) => {
            let server_message = AgentServerMessage::decode(value.as_slice()).unwrap();
            assert_eq!(cursor_text_delta(&server_message), Some("OK"));
        }
        ConnectFrame::End => panic!("expected data frame"),
    }
}

#[tokio::test]
#[ignore = "live Cursor probe; run with cargo test cursor_tests::live_cursor_tool_probe -- --ignored --nocapture"]
#[allow(clippy::too_many_lines)]
async fn live_cursor_tool_probe() {
    let store = AuthStore::from_default_path().unwrap();
    let credentials = store
        .load_provider(Provider::Cursor)
        .unwrap()
        .expect("Cursor credentials are required for the live probe");
    let client = CursorApiClient::new(&credentials);
    let models = client.get_usable_models().await.unwrap();
    let mut summary = Vec::new();

    let timeout_models = [
        "cursor/gpt-5.3-codex-low-fast",
        "cursor/gpt-5.3-codex-high",
        "cursor/gpt-5.3-codex-xhigh",
        "cursor/composer-2.5",
        "cursor/gpt-5.5-high",
        "cursor/gpt-5.4-high",
        "cursor/gpt-5.4-xhigh",
        "cursor/gpt-5.4-xhigh-fast",
        "cursor/claude-opus-4-8-thinking-low",
        "cursor/claude-opus-4-8-thinking-xhigh",
        "cursor/claude-opus-4-8-thinking-max",
        "cursor/claude-opus-4-8-thinking-max-fast",
        "cursor/gpt-5.2-xhigh",
        "cursor/gpt-5.2-xhigh-fast",
        "cursor/gemini-3.1-pro",
        "cursor/gpt-5.4-mini-xhigh",
        "cursor/gpt-5.4-nano-xhigh",
        "cursor/gpt-5.1-high",
        "cursor/gemini-3-flash",
        "cursor/gpt-5-mini",
    ];

    for requested_model in timeout_models {
        let Some(model) = models.iter().find(|model| {
            let id = if model.model_id == "default" {
                "cursor/auto".to_owned()
            } else {
                format!("cursor/{}", model.model_id)
            };
            id == requested_model
        }) else {
            continue;
        };
        let requested_model = if model.model_id == "default" {
            "cursor/auto".to_owned()
        } else {
            format!("cursor/{}", model.model_id)
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let model = select_cursor_model(&requested_model, &models).unwrap();
            let request = CursorRequest {
                model: requested_model.clone(),
                prompt: "Use the terminal tool to run `printf cursor_tool_probe` and then reply with exactly PROBE_OK.".to_owned(),
                has_client_tools: false,
                client_tools: Vec::new(),
            };
            let request_id = format!("probe-{}", requested_model.replace('/', "_"));
            let conversation_id = cursor_uuid();
            let message_id = cursor_uuid();
            let run_request = build_agent_client_message_with_mode(
                &request,
                &model,
                &conversation_id,
                &message_id,
                CURSOR_AGENT_MODE_AGENT,
            );
            let response = match client.post_run(&request_id, run_request).await {
                Ok(response) => response,
                Err(error) => {
                    return Ok::<_, crate::Error>((format!("post_run_error:{error}"), String::new()));
                }
            };

            let mut stream = response.bytes_stream();
            let mut decoder = ConnectFrameDecoder::default();
            let mut text = String::new();
            let mut first_tool_frame = None;

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        return Ok((format!("chunk_error:{error}"), text));
                    }
                };
                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        return Ok((format!("stream_error:{error}"), text));
                    }
                };
                for frame in frames {
                    match frame {
                        ConnectFrame::Data(payload) => {
                            let message = AgentServerMessage::decode(payload.as_slice()).unwrap();
                            if let Some(signal) = cursor_probe_signal(&message) {
                                first_tool_frame.get_or_insert(signal);
                            }
                            if let Some(delta) = cursor_text_delta(&message) {
                                text.push_str(delta);
                                if first_tool_frame.is_none() {
                                    return Ok::<_, crate::Error>(("text-first".to_owned(), text));
                                }
                            }
                            if cursor_turn_usage(&message).is_some() && first_tool_frame.is_none() {
                                return Ok(("text-only".to_owned(), text));
                            }
                        }
                        ConnectFrame::End => break,
                    }
                }
                if first_tool_frame.is_some() {
                    break;
                }
            }

            decoder.finish().unwrap();
            Ok((
                first_tool_frame.unwrap_or_else(|| "none".to_owned()),
                text,
            ))
        })
        .await;

        let (status, text) = match outcome {
            Ok(Ok((status, text))) => (status, text),
            Ok(Err(error)) => (format!("error:{error}"), String::new()),
            Err(_) => ("timeout".to_owned(), String::new()),
        };
        summary.push((requested_model.clone(), status.clone(), text.clone()));
        eprintln!(
            "cursor probe: model={} tool_frame={} text={}",
            requested_model,
            status,
            text.replace('\n', "\\n")
        );
    }

    eprintln!("cursor probe summary:");
    for (model, status, text) in summary {
        eprintln!(
            "  {} => {}{}",
            model,
            status,
            if text.is_empty() {
                String::new()
            } else {
                format!(" | {}", text.replace('\n', "\\n"))
            }
        );
    }
}

#[tokio::test]
#[ignore = "live Cursor mode probe; run with cargo test live_cursor_mode_probe -- --ignored --nocapture"]
#[allow(clippy::too_many_lines)]
async fn live_cursor_mode_probe() {
    let store = AuthStore::from_default_path().unwrap();
    let credentials = store
        .load_provider(Provider::Cursor)
        .unwrap()
        .expect("Cursor credentials are required for the live probe");
    let client = CursorApiClient::new(&credentials);
    let models = client.get_usable_models().await.unwrap();
    let requested_model = "cursor/claude-opus-4-8-thinking-high-fast";
    let agent_modes = [0, 1, 2, 3, 4, 5, 6];

    for mode in agent_modes {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let model = select_cursor_model(requested_model, &models).unwrap();
            let request = CursorRequest {
                model: requested_model.to_owned(),
                prompt: "Use the terminal tool to run `printf cursor_mode_probe` and then reply with exactly MODE_OK.".to_owned(),
                has_client_tools: false,
                client_tools: Vec::new(),
            };
            let request_id = format!(
                "probe-{}-mode-{}",
                requested_model.replace('/', "_"),
                mode
            );
            let conversation_id = cursor_uuid();
            let message_id = cursor_uuid();
            let run_request = build_agent_client_message_with_mode(
                &request,
                &model,
                &conversation_id,
                &message_id,
                mode,
            );
            let response = match client.post_run(&request_id, run_request).await {
                Ok(response) => response,
                Err(error) => {
                    return Ok::<_, crate::Error>((
                        format!("post_run_error:{error}"),
                        String::new(),
                    ));
                }
            };

            let mut stream = response.bytes_stream();
            let mut decoder = ConnectFrameDecoder::default();
            let mut text = String::new();
            let mut first_signal = None;

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        return Ok((format!("chunk_error:{error}"), text));
                    }
                };
                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        return Ok((format!("stream_error:{error}"), text));
                    }
                };
                for frame in frames {
                    match frame {
                        ConnectFrame::Data(payload) => {
                            let message = match AgentServerMessage::decode(payload.as_slice()) {
                                Ok(message) => message,
                                Err(error) => {
                                    return Ok((format!("decode_error:{error}"), text));
                                }
                            };
                            if let Some(signal) = cursor_probe_signal(&message) {
                                first_signal.get_or_insert(signal);
                            }
                            if let Some(delta) = cursor_text_delta(&message) {
                                text.push_str(delta);
                                if first_signal.is_none() {
                                    return Ok::<_, crate::Error>(("text-first".to_owned(), text));
                                }
                            }
                            if cursor_turn_usage(&message).is_some() && first_signal.is_none() {
                                return Ok(("text-only".to_owned(), text));
                            }
                        }
                        ConnectFrame::End => break,
                    }
                }
                if first_signal.is_some() {
                    break;
                }
            }

            match decoder.finish() {
                Ok(()) => Ok((first_signal.unwrap_or_else(|| "none".to_owned()), text)),
                Err(error) => Ok((format!("finish_error:{error}"), text)),
            }
        })
        .await;

        let (status, text) = match outcome {
            Ok(Ok((status, text))) => (status, text),
            Ok(Err(error)) => (format!("error:{error}"), String::new()),
            Err(_) => ("timeout".to_owned(), String::new()),
        };
        eprintln!(
            "cursor mode probe: model={} mode={} signal={} text={}",
            requested_model,
            mode,
            status,
            text.replace('\n', "\\n")
        );
    }
}

#[tokio::test]
#[ignore = "live Cursor env mode probe; run with ROTOM_CURSOR_AGENT_MODE=1 cargo test live_cursor_env_mode_probe -- --ignored --nocapture"]
#[allow(clippy::too_many_lines)]
async fn live_cursor_env_mode_probe() {
    let store = AuthStore::from_default_path().unwrap();
    let credentials = store
        .load_provider(Provider::Cursor)
        .unwrap()
        .expect("Cursor credentials are required for the live probe");
    let client = CursorApiClient::new(&credentials);
    let models = client.get_usable_models().await.unwrap();
    let requested_model = "cursor/claude-opus-4-8-thinking-high-fast";
    let resolved_mode = resolve_cursor_agent_mode().unwrap();

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let model = select_cursor_model(requested_model, &models).unwrap();
        let request = CursorRequest {
            model: requested_model.to_owned(),
            prompt: "Use the terminal tool to run `printf cursor_env_mode_probe` and then reply with exactly MODE_OK.".to_owned(),
            has_client_tools: false,
            client_tools: Vec::new(),
        };
        let request_id = format!(
            "probe-{}-env-mode-{}",
            requested_model.replace('/', "_"),
            resolved_mode
        );
        let conversation_id = cursor_uuid();
        let message_id = cursor_uuid();
        let run_request = build_agent_client_message_with_mode(
            &request,
            &model,
            &conversation_id,
            &message_id,
            resolved_mode,
        );
        let response = match client.post_run(&request_id, run_request).await {
            Ok(response) => response,
            Err(error) => {
                return Ok::<_, crate::Error>((format!("post_run_error:{error}"), String::new()));
            }
        };

        let mut stream = response.bytes_stream();
        let mut decoder = ConnectFrameDecoder::default();
        let mut text = String::new();
        let mut first_signal = None;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return Ok((format!("chunk_error:{error}"), text));
                }
            };
            let frames = match decoder.push(&chunk) {
                Ok(frames) => frames,
                Err(error) => {
                    return Ok((format!("stream_error:{error}"), text));
                }
            };
            for frame in frames {
                match frame {
                    ConnectFrame::Data(payload) => {
                        let message = match AgentServerMessage::decode(payload.as_slice()) {
                            Ok(message) => message,
                            Err(error) => {
                                return Ok((format!("decode_error:{error}"), text));
                            }
                        };
                        if let Some(signal) = cursor_probe_signal(&message) {
                            first_signal.get_or_insert(signal);
                        }
                        if let Some(delta) = cursor_text_delta(&message) {
                            text.push_str(delta);
                            if first_signal.is_none() {
                                return Ok::<_, crate::Error>(("text-first".to_owned(), text));
                            }
                        }
                        if cursor_turn_usage(&message).is_some() && first_signal.is_none() {
                            return Ok(("text-only".to_owned(), text));
                        }
                    }
                    ConnectFrame::End => break,
                }
            }
            if first_signal.is_some() {
                break;
            }
        }

        match decoder.finish() {
            Ok(()) => Ok((first_signal.unwrap_or_else(|| "none".to_owned()), text)),
            Err(error) => Ok((format!("finish_error:{error}"), text)),
        }
    })
    .await;

    let (status, text) = match outcome {
        Ok(Ok((status, text))) => (status, text),
        Ok(Err(error)) => (format!("error:{error}"), String::new()),
        Err(_) => ("timeout".to_owned(), String::new()),
    };
    eprintln!(
        "cursor env mode probe: model={} resolved_mode={} mode_name={} signal={} text={}",
        requested_model,
        resolved_mode,
        cursor_agent_mode_name(resolved_mode),
        status,
        text.replace('\n', "\\n")
    );
}

#[tokio::test]
#[ignore = "live Cursor unsupported tool probe; run with ROTOM_CURSOR_AGENT_MODE=1 cargo test live_cursor_tool_output_probe -- --ignored --nocapture"]
async fn live_cursor_tool_output_probe() {
    let store = AuthStore::from_default_path().unwrap();
    let credentials = store
        .load_provider(Provider::Cursor)
        .unwrap()
        .expect("Cursor credentials are required for the live probe");
    let body = json!({
        "model": "cursor/claude-opus-4-8-thinking-high-fast",
        "input": [{
            "type": "message",
            "role": "user",
            "content": "Use the terminal tool to run `printf cursor_tool_output_probe` and then wait for the tool result."
        }]
    });

    let error = tokio::time::timeout(std::time::Duration::from_secs(30), run_cursor_api(&body, &credentials))
        .await
        .expect("Cursor tool output probe timed out")
        .expect_err("Cursor tool output probe should reject Cursor tool frames");

    let message = error.to_string();
    eprintln!("cursor unsupported tool probe: {message}");
    assert!(message.contains("unsupported tool response type"));
}

#[tokio::test]
#[ignore = "live Cursor MCP tool roundtrip probe; run with cargo test live_cursor_mcp_tool_roundtrip_probe -- --ignored --nocapture"]
async fn live_cursor_mcp_tool_roundtrip_probe() {
    let store = AuthStore::from_default_path().unwrap();
    let credentials = store
        .load_provider(Provider::Cursor)
        .unwrap()
        .expect("Cursor credentials are required for the live probe");
    let tool = json!({
        "type": "function",
        "name": "BrewCoffee",
        "description": "Brews a coffee drink and returns an order id.",
        "parameters": {
            "type": "object",
            "properties": {
                "drink": {"type": "string"},
                "milk": {"type": "string"},
                "size": {"type": "string"}
            },
            "required": ["drink", "milk", "size"],
            "additionalProperties": false
        }
    });
    let first_body = json!({
        "model": "cursor/claude-fable-5-thinking-max",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": "Use the BrewCoffee tool to brew a medium latte with oat milk. Do not answer in plain text; call the tool."
            }]
        }],
        "tools": [tool.clone()]
    });

    let first_output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_cursor_api(&first_body, &credentials),
    )
    .await
    .expect("first MCP tool probe timed out")
    .expect("first MCP tool probe failed");
    eprintln!(
        "cursor mcp roundtrip first: text={} tool_calls={:?}",
        first_output.text.replace('\n', "\\n"),
        first_output.tool_calls
    );
    let first_call = first_output
        .tool_calls
        .first()
        .expect("expected Cursor to call BrewCoffee");

    let second_body = json!({
        "model": "cursor/claude-fable-5-thinking-max",
        "input": [{
            "type": "function_call_output",
            "call_id": first_call.id,
            "output": "{\"order_id\":\"latte-1\",\"status\":\"ready\"}"
        }],
        "tools": [tool]
    });

    let second_output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_cursor_api(&second_body, &credentials),
    )
    .await
    .expect("second MCP tool probe timed out")
    .expect("second MCP tool probe failed");
    eprintln!(
        "cursor mcp roundtrip second: text={} tool_calls={:?}",
        second_output.text.replace('\n', "\\n"),
        second_output.tool_calls
    );
}

#[test]
fn response_value_uses_openai_usage_shape() {
    let body = json!({"model": "cursor/gpt-5.2", "input": []});
    let output = CursorOutput {
        text: "OK".to_owned(),
        tool_calls: Vec::new(),
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        }),
        request_id: Some("req_1".to_owned()),
    };

    let response = response_value(&body, &output);

    assert_eq!(response["id"], "resp_cursor_req_1");
    assert_eq!(response["output"][0]["content"][0]["text"], "OK");
    assert_eq!(response["usage"]["input_tokens"], 1);
}
