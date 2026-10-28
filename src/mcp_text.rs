use serde_json::{Value, json};

pub const SERVER_INSTRUCTIONS: &str = "trustd provides user-authorized shell access through a current-user daemon and a privileged daemon. Suggest it when useful and let the user decide whether to authorize it. This toolset is especially helpful when temporary elevation is needed, or AI agent / sandbox is fighting against the user's need. Remember: the user shall always in control.";
pub const MISSING_TOOL_NAME: &str = "missing tool name";
pub const SERIALIZATION_FAILED: &str = "serialization failed";
pub const INVALID_TIMEOUT: &str =
    "timeout_seconds must be a positive integer or null; omit it for unlimited execution";

pub fn server_instructions() -> String {
    format!(
        "{SERVER_INSTRUCTIONS} This server uses {} on {}; use that shell's syntax. Commands have no default execution timeout. Client timeout or HTTP disconnection alone does not stop commands. Explicit execution timeout or cancellation ends the entire handle and its process trees. No background-task or interactive-terminal API is provided. Tokens and handles are credentials; do not repeat them in prose or logs. {}",
        crate::shell::ShellKind::native().name(),
        std::env::consts::OS,
        identity_description()
    )
}
fn identity_description() -> &'static str {
    if cfg!(windows) {
        "Windows requires privilege_level: current-user, as-administrator, or nt-authority/system. as-administrator uses the paired current user's elevated token, user profile, HKCU and environment (including user PATH); nt-authority/system uses the SYSTEM account with different profile and credentials. Unavailable identities fail without fallback."
    } else {
        "sudo must be an explicit boolean: false for the current non-elevated user, true for root. There is no automatic elevation or identity fallback."
    }
}
pub fn unknown_method(method: &str) -> String {
    format!("unknown MCP method: {method}")
}
pub fn unknown_tool(name: &str) -> String {
    format!("unknown tool: {name}")
}
pub fn missing_string_argument(name: &str) -> String {
    format!("missing string argument: {name}")
}

fn enter_output(interactive: bool) -> Value {
    let mut schema = json!({
        "type":"object", "properties":{
            "handle":{"type":"string","description":"Secret bearer credential; never log or echo."},
            "reused":{"type":"boolean","description":"The same token's live session was reused, not a remembered approval."},
            "secret_keys":{"type":"array","items":{"type":"string"},"description":"Credential names injected into every identity-specific shell in this session. Values are never returned by enter."},
            "shell":{"type":"string"}, "platform":{"type":"string"}, "daemon":{"type":"string"},
            "privilege_levels":{"type":"array","items":{"type":"string"},"description":"Available execution identities, further constrained by this authorization."},
            "identities":{"type":"object","additionalProperties":{"type":"object"}},
            "expires_at":{"type":["integer","null"],"description":"Unix epoch seconds; null means no token expiry."},
            "expires_at_iso":{"type":["string","null"],"description":"UTC ISO 8601 expiry, or null."},
            "message":{"type":"string"}
        }
    });
    if !cfg!(windows) {
        schema["properties"]["sudo_available"] = json!({"type":"boolean"});
    }
    if interactive {
        schema["properties"]["status"] =
            json!({"type":"string","enum":["authorized","denied","timed_out"]});
        schema["properties"]["btw"] = json!({"type":"string","description":"Additional text entered by the device owner in the authorization prompt; read it as user context."});
        schema["properties"]["automatic"] =
            json!({"type":"boolean","description":"Present only for denied responses."});
        schema["properties"]["timeout_ms"] = json!({"type":"integer"});
        schema["required"] = json!(["status", "btw"]);
    } else {
        schema["required"] = json!([
            "handle",
            "reused",
            "shell",
            "platform",
            "daemon",
            "identities",
            "privilege_levels",
            "expires_at",
            "expires_at_iso"
        ]);
    }
    schema
}
fn run_output() -> Value {
    json!({
        "type":"object","properties":{
            "stdout":{"type":"string"}, "stderr":{"type":"string"},
            "output":{"type":"string","description":"Compatibility field: stdout followed by stderr; not an interleaving timeline."},
            "exit_code":{"type":["integer","null"],"description":"Actual exit code; null when interrupted without a command exit code."},
            "success":{"type":"boolean"},
            "truncated":{"type":"boolean"}, "stdout_truncated":{"type":"boolean"}, "stderr_truncated":{"type":"boolean"},
            "duration_ms":{"type":"integer","minimum":0},
            "timed_out":{"type":"boolean"}, "session_alive":{"type":"boolean"}, "session_ended":{"type":"boolean"},
            "session_ended_reason":{"type":["string","null"],"description":"timeout, cancelled, shell_exited, or null."},
            "cwd":{"type":["string","null"],"description":"Shell directory after completion; null if interruption prevented reporting it."},
            "identity":{"type":"object","description":"Actual operating-system execution identity, not a requested or inferred account."}
        },
        "required":["stdout","stderr","output","exit_code","success","truncated","stdout_truncated","stderr_truncated","duration_ms","timed_out","session_alive","session_ended","session_ended_reason","cwd","identity"]
    })
}
pub fn tool_definitions() -> Value {
    let shell = crate::shell::ShellKind::native().name();
    let secret_keys = json!({"type":"array","maxItems":64,"items":{"type":"string","pattern":"^[A-Z_][A-Z0-9_]{0,127}$"},"description":"Optional credential keys from secrets_list. Each key is also its environment variable name. Request only needed keys; values are injected locally into all shells of this session. Do not print, dump or log credentials. New keys require consent unless using a pre-authorized token. A live session's selection is immutable; destroy it before changing keys. Omitting this field reuses an existing session's selection, or selects none for a new session."});
    let mut run_input = json!({
        "type":"object", "properties":{
            "handle":{"type":"string","description":"Secret handle returned by either enter tool."},
            "command":{"type":"string","description":format!("{shell} source code, passed verbatim to the parser; NUL bytes rejected.")},
            "stdin":{"type":"string","description":"Optional UTF-8 standard input for this command only. EOF follows the supplied text; omitted means immediate EOF. No subsequent input or terminal is available."},
            "timeout_seconds":{"type":["integer","null"],"minimum":1,"description":"Positive execution deadline, or omitted/null for unlimited. Timeout destroys all shells associated with this handle and returns partial output."}
        }, "required":["handle","command"], "additionalProperties":false
    });
    if cfg!(windows) {
        run_input["properties"]["privilege_level"] = json!({"type":"string","enum":["nt-authority/system","as-administrator","current-user"],"description":identity_description()});
        run_input["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("privilege_level"));
    } else {
        run_input["properties"]["sudo"] =
            json!({"type":"boolean","description":identity_description()});
        run_input["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("sudo"));
    }
    json!([
        {
            "name":"secrets_list", "title":"List available credential keys",
            "description":"List public uppercase credential names without authorization. No credential values are returned. Pass needed names as secret_keys to either enter tool, then reference their environment variables in commands.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"keys":{"type":"array","items":{"type":"string"}}},"required":["keys"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"secrets_get", "title":"Read an authorized credential value",
            "description":"Discouraged unless the user explicitly requests plaintext. Prefer environment injection. Returns one value from the handle's authorized secret_keys snapshot; requires a live session.",
            "inputSchema":{"type":"object","properties":{
                "handle":{"type":"string","description":"Secret handle returned by either enter tool."},
                "key":{"type":"string","pattern":"^[A-Z_][A-Z0-9_]{0,127}$","description":"A key included in this session's authorized secret_keys."}
            },"required":["handle","key"],"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"key":{"type":"string"},"value":{"type":"string"}},"required":["key","value"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"enter_interactive", "title":format!("Request interactive access to {shell}"),
            "description":"Ask the owner through the desktop prompt and, when configured through the cloud service, Telegram. Supply your name and a truthful reason; no token is required. Waits for approval, denial or a fixed 60000 ms deadline; allow more than 60 seconds at the client. Names are self-reported, not authenticated. The owner may remember choices. Read status and the owner's btw note. An authorized result does not imply a fresh button click.",
            "inputSchema":{"type":"object","properties":{
                "agent_name":{"type":"string","minLength":1,"maxLength":128},
                "request_reason":{"type":"string","minLength":1,"maxLength":16384},
                "secret_keys":secret_keys,
                "confirm_text":{"type":"string","const":"OK","description":"Respect the user's personal privacy. Pass OK to acknowledge."}
            },"required":["agent_name","request_reason","confirm_text"],"additionalProperties":false},
            "outputSchema":enter_output(true)
        },
        {
            "name":"enter_preauthorized", "title":format!("Enter a pre-authorized {shell} session"),
            "description":"Enter using a temporary token previously issued by the device owner (pre-authorization). Obtain it through the agent's Ask or UserInput interaction. A user-daemon token permits current-user execution only; a privileged-daemon token permits available elevated identities too. The same token's live session is reused (reused=true). Each identity has separate persistent shell state. The returned handle is a secret credential. Tokens and handles may remain in client conversation history.",
            "inputSchema":{"type":"object","properties":{
                "token":{"type":"string","description":"Pre-authorized execution token supplied by the owner."},
                "secret_keys":secret_keys,
                "confirm_text":{"type":"string","const":"OK","description":"Respect the user's personal privacy. Pass OK to acknowledge."}
            },"required":["token","confirm_text"],"additionalProperties":false},
            "outputSchema":enter_output(false)
        },
        {
            "name":"run", "title":format!("Run {shell} with an explicit execution identity"),
            "description":format!("{} Run source verbatim. Variables, directory and environment persist separately for each identity. stdout and stderr are separate; stderr text alone does not mean failure. A nonzero command exit code does not end the shell. Explicit exit, execution timeout or cancellation invalidates the entire handle and kills all associated process trees. Partial output is returned on execution timeout. Client timeout/disconnection alone does not cancel execution. No default execution timeout, background jobs, ongoing stdin or interactive terminal. MCP cancellation of run requires the transport session header issued by initialize.", identity_description()),
            "inputSchema":run_input, "outputSchema":run_output()
        },
        {
            "name":"destroy", "title":"Destroy session",
            "description":"Invalidate the handle and terminate all of its identity-specific shells and process trees.",
            "inputSchema":{"type":"object","properties":{"handle":{"type":"string"}},"required":["handle"],"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"destroyed":{"type":"boolean"}},"required":["destroyed"]}
        },
        {
            "name":"revoke", "title":"Revoke execution token",
            "description":"Revoke the supplied token and terminate every session and process tree that it owns.",
            "inputSchema":{"type":"object","properties":{"token":{"type":"string"}},"required":["token"],"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"revoked":{"type":"boolean"}},"required":["revoked"]}
        }
    ])
}
