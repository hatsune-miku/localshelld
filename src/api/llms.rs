use std::fmt::Write;

use axum::extract::State;

use crate::{mcp_text::server_instructions, server::AppState};

/// Public documentation uses only static schemas and non-sensitive daemon metadata.
/// Returning String makes Axum serve text/plain; charset=utf-8, not the SPA fallback.
pub(super) async fn document(State(state): State<AppState>) -> String {
    let mut text = String::from(include_str!("../../docs/llms.md"));
    let mode = serde_json::to_value(state.mode).expect("daemon mode is serializable");
    write!(
        text,
        "\n## This daemon\n\nBase URL: http://{}\nDaemon: {}\nPlatform: {}\nVersion: {}\nCommit: {}\n\n{}\n\n## Operations\n",
        state.config.bind,
        mode.as_str().expect("daemon mode is a string"),
        std::env::consts::OS,
        crate::VERSION,
        crate::COMMIT,
        server_instructions(),
    )
    .expect("writing to String cannot fail");
    for operation in super::operation_definitions()
        .as_array()
        .expect("static operation definitions")
    {
        write!(
            text,
            "\n### {} {}\n\nMCP tool: `{}`\n\n{}\n\nInput JSON Schema:\n\n```json\n{}\n```\n\nOutput JSON Schema:\n\n```json\n{}\n```\n",
            operation["method"].as_str().unwrap(),
            operation["path"].as_str().unwrap(),
            operation["name"].as_str().unwrap(),
            operation["description"].as_str().unwrap(),
            serde_json::to_string_pretty(&operation["inputSchema"]).unwrap(),
            serde_json::to_string_pretty(&operation["outputSchema"]).unwrap(),
        )
        .expect("writing to String cannot fail");
    }
    text
}
