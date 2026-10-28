# localshelld API

> Local shell execution over HTTP and MCP, authorized by the device owner.

## Discovery and access

- `GET /llms.txt` returns this self-contained, AI-readable document as UTF-8 plain text. No token, handle, Authorization header, cookie, or management login is required on either daemon.
- Default user daemon: `http://127.0.0.1:32120`. Default privileged daemon: `http://127.0.0.1:32119`. Both expose the same routes; use the base URL of the daemon you are calling. Ports are configurable.
- `GET /health` returns `status`, `service`, `daemon`, `version`, and `commit` without credentials.
- `GET /v1/capabilities` returns protocol version, daemon/platform/build metadata, operations with HTTP methods/paths and input/output JSON Schemas, cancellation details, and `interactive_timeout_ms` (60000), without credentials.
- The operation schemas below are generated from the same definitions as `/v1/capabilities` and MCP `tools/list`, for this build's operating system.
- The service listens on loopback. Host must match a loopback address and the configured port. If Origin is sent, it must be HTTP loopback with that port. Cross-origin browser access is not enabled. Documentation access does not grant execution or management access.

## HTTP workflow

Send POST bodies as JSON with `Content-Type: application/json`. Send the operation's input object directly, without a JSON-RPC wrapper. GET operations do not need a body. Successful operations return the output object directly. Tokens and handles belong in JSON bodies, never URLs or logs; these execution routes do not use a Bearer Authorization header.

1. Optionally call `GET /v1/secrets/keys` to discover credential names. Only names are returned, never values. Pass only needed names as `secret_keys` when entering a session.
2. Call `POST /v1/sessions/enter/interactive` with `{"agent_name":"Example Agent","request_reason":"Inspect the current execution account","confirm_text":"OK"}`. Allow more than 60 seconds at the client. Inspect `status`: only `authorized` supplies a handle; `denied` and `timed_out` also return HTTP 200. Read the owner's `btw` note. `confirm_text` acknowledges privacy, not approval. A remembered choice may be used; do not assume a new button click occurred.
3. Alternatively, call `POST /v1/sessions/enter/preauthorized` with `{"token":"<OWNER_SUPPLIED_TOKEN>","confirm_text":"OK"}`. Use an execution token supplied by the owner, not a management password or management session. A live session for the same token is reused (`reused:true`).
4. Keep the returned `handle` private. Check `privilege_levels` and `identities` before choosing an execution identity. Windows requires `privilege_level`; Linux/macOS require an explicit boolean `sudo`. Unavailable identities fail without fallback.
5. Call `POST /v1/commands/run`. Windows example: `{"handle":"<HANDLE>","command":"whoami","privilege_level":"current-user","timeout_seconds":10}`. Linux/macOS example: `{"handle":"<HANDLE>","command":"id","sudo":false,"timeout_seconds":10}`. Use only the variant matching this daemon's platform and schema.
6. Call `POST /v1/sessions/destroy` with `{"handle":"<HANDLE>"}` when finished. To revoke an execution token and all its sessions, call `POST /v1/tokens/revoke` with `{"token":"<OWNER_SUPPLIED_TOKEN>"}`.

Optional `secret_keys` are injected as environment variables into every identity-specific shell in the session. Use `$env:KEY` in PowerShell or `$KEY` in Bash without printing credential values. Omitting `secret_keys` or passing an empty list preserves a reused session's existing selection. A different nonempty selection requires destroying the session and entering again.

## Execution, errors, and cancellation

Variables, current directory, and environment persist separately for each execution identity. Commands use the platform's native shell syntax. `stdin` is optional UTF-8 text for this command only, followed by EOF; omission means immediate EOF. There is no ongoing stdin, interactive terminal, or background-task API.

`timeout_seconds` is a positive integer, or null/omitted for unlimited execution. Client timeout or HTTP disconnection alone does not stop execution. Do not automatically retry a command whose response was lost: it may still be running or may already have completed. To cancel, send a separate `POST /v1/sessions/destroy` request; this invalidates the entire handle and terminates its shells and process trees, not just one command. Execution timeout also ends the whole session and returns partial output.

Inspect `exit_code` and `success`; stderr text alone does not imply failure. `stdout` and `stderr` are separate. `output` concatenates stdout then stderr and is not a chronological merge. Nonzero command exit codes normally keep the session alive; exiting the shell ends it. Check `session_alive`, `session_ended`, and `session_ended_reason` (`timeout`, `cancelled`, `shell_exited`, or null). `identity` reports the actual shell OS account; `cwd` may be null after interruption.

Invalid arguments return HTTP 400, invalid execution credentials 401, disallowed execution identities 403, and unavailable authorization channels 503. Application errors use `{"error":"message"}`. JSON parsing, Content-Type, and extraction errors can return 400/415/422 with a non-JSON body. Unknown fields and wrong types are rejected, not coerced. HTTP 200 does not by itself mean an interactive request was approved or a shell command succeeded.

## MCP transport

`POST /mcp` accepts JSON-RPC 2.0 over Streamable HTTP (protocol version `2025-06-18`). Initialize, preserve the returned `Mcp-Session-Id` header, and send `notifications/initialized`. Discover schemas with `tools/list`; invoke the same operations with `tools/call`, for example `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run","arguments":{"handle":"<HANDLE>","command":"whoami","privilege_level":"current-user"}}}` on Windows. Results include `structuredContent` and a text content block. HTTP and MCP share execution tokens, handles, validation, and authorization decisions.

For MCP run cancellation, send `notifications/cancelled` with `params.requestId` and the same `Mcp-Session-Id` header as the in-flight run. It destroys the entire execution handle and its process trees. The transport session header is not an execution credential. Interactive authorization has its own fixed 60-second deadline and is not cancelled by this notification.

The `/v1/admin/*` management routes use separate management authentication and are not needed for the execution workflow. `/v1/bridge` is an internal paired-daemon protocol, not a public client API. The legacy `/v1/sessions/enter` route remains for compatibility; new clients should use the documented preauthorized route.
