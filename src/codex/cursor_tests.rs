use super::*;
use serde_json::json;

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
        "System:\nBe concise.\n\nUser:\nHello\n\nAssistant:\nHi\n\nTool result call_1:\n42"
    );
}

#[test]
fn rejects_cursor_tools_without_none_choice() {
    let body = json!({
        "model": "cursor/auto",
        "input": [{"type": "message", "role": "user", "content": "Hello"}],
        "tool_choice": "auto",
        "tools": [{"type": "function", "name": "lookup"}]
    });

    let error = CursorRequest::from_body(&body).unwrap_err();

    assert!(error.to_string().contains("client-supplied tools"));
}

#[test]
fn allows_tools_when_tool_choice_is_none() {
    let body = json!({
        "model": "cursor/auto",
        "input": [{"type": "message", "role": "user", "content": "Hello"}],
        "tool_choice": "none",
        "tools": [{"type": "function", "name": "lookup"}]
    });

    let request = CursorRequest::from_body(&body).unwrap();

    assert_eq!(request.prompt, "User:\nHello");
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

#[test]
fn response_value_uses_openai_usage_shape() {
    let body = json!({"model": "cursor/gpt-5.2", "input": []});
    let output = CursorOutput {
        text: "OK".to_owned(),
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
