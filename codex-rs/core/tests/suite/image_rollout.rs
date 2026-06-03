use anyhow::Context;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use image::ImageBuffer;
use image::Rgba;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::time::Duration;

fn message_has_input_image(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { content, .. } => content
            .iter()
            .any(|span| matches!(span, ContentItem::InputImage { .. })),
        _ => false,
    }
}

fn find_user_message_with_text(text: &str, needle: &str) -> Option<ResponseItem> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rollout: RolloutLine = match serde_json::from_str(trimmed) {
            Ok(rollout) => rollout,
            Err(_) => continue,
        };
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) =
            &rollout.item
            && role == "user"
            && content.iter().any(
                |span| matches!(span, ContentItem::InputText { text } if text.contains(needle)),
            )
            && let RolloutItem::ResponseItem(item) = rollout.item.clone()
        {
            return Some(item);
        }
    }
    None
}

async fn read_rollout_text(path: &Path) -> anyhow::Result<String> {
    for _ in 0..50 {
        if path.exists()
            && let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return Ok(text);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("read rollout file at {}", path.display()))
}

fn write_test_png(path: &Path, color: [u8; 4]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let image = ImageBuffer::from_pixel(2, 2, Rgba(color));
    image.save(path)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_paste_local_image_persists_rollout_request_shape() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex {
        codex,
        cwd,
        session_configured,
        home: _home,
        ..
    } = test_codex().build(&server).await?;

    let rel_path = "images/paste.png";
    let abs_path = cwd.path().join(rel_path);
    write_test_png(&abs_path, [12, 34, 56, 255])?;

    let response = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ]);
    responses::mount_sse_once(&server, response).await;

    let session_model = session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::LocalImage {
                    path: abs_path.clone(),
                    detail: None,
                },
                UserInput::Text {
                    text: "pasted image".to_string(),
                    text_elements: Vec::new(),
                },
            ],
            environments: None,
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let rollout_path = codex.rollout_path().expect("rollout path");
    let rollout_text = read_rollout_text(&rollout_path).await?;
    let actual = find_user_message_with_text(&rollout_text, "call the `view_image` tool")
        .expect("expected user message with view_image instruction in rollout");

    let expected = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: codex_protocol::models::local_image_tool_instruction_text(
                    /*label_number*/ 1, &abs_path,
                ),
            },
            ContentItem::InputText {
                text: "pasted image".to_string(),
            },
        ],
        phase: None,
    };

    assert_eq!(actual, expected);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drag_drop_image_persists_rollout_request_shape() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let TestCodex {
        codex,
        cwd,
        session_configured,
        home: _home,
        ..
    } = test_codex().build(&server).await?;

    let image_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGNgYAAAAAMAASsJTYQAAAAASUVORK5CYII=".to_string();

    let response = sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ]);
    responses::mount_sse_once(&server, response).await;

    let session_model = session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .submit(Op::UserInput {
            items: vec![
                UserInput::Image {
                    image_url: image_url.clone(),
                    detail: None,
                },
                UserInput::Text {
                    text: "dropped image".to_string(),
                    text_elements: Vec::new(),
                },
            ],
            environments: None,
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                cwd: Some(cwd.path().to_path_buf()),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let rollout_path = codex.rollout_path().expect("rollout path");
    let rollout_text = read_rollout_text(&rollout_path).await?;
    let actual = find_user_message_with_text(
        &rollout_text,
        "native image input is disabled and this attachment does not have a local filesystem path",
    )
    .expect("expected user message with disabled native image instruction in rollout");

    assert!(
        !message_has_input_image(&actual),
        "image URL attachment should not be sent as native InputImage"
    );
    let expected = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![
            ContentItem::InputText {
                text: codex_protocol::models::image_tool_unavailable_instruction_text(
                    /*label_number*/ 1,
                ),
            },
            ContentItem::InputText {
                text: "dropped image".to_string(),
            },
        ],
        phase: None,
    };

    assert_eq!(actual, expected);

    Ok(())
}
