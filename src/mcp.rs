//! `lessons mcp`: a minimal Model Context Protocol server over stdio, with exactly two tools.
//!
//! Each line on stdin is one JSON-RPC 2.0 message and each reply is one line on stdout. Nothing
//! else may be written to stdout, so logs go to stderr. It implements the 2025-06-18 protocol and
//! the two before it: the initialize handshake, ping, tools/list and tools/call. Two tools only,
//! on purpose: every tool's schema sits in the agent's context, so each extra tool costs tokens.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::db::{self, Domain};
use crate::store::{self, NewLesson, SearchHit};
use crate::{config, journal, query};

/// Protocol versions this server implements, newest first. A client asking for one of them gets
/// it back; any other request is answered with the newest, and the client decides whether to stay.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// A protocol-level failure, answered as a JSON-RPC error rather than as a tool result.
struct RequestError {
    code: i64,
    message: String,
}

/// Reads messages from `client_input` until the client closes it, answering on `client_output`.
pub fn serve(
    lessons_home: &Path,
    client_input: impl BufRead,
    mut mut_client_output: impl Write,
) -> Result<()> {
    journal!("mcp: serving {}", lessons_home.display());
    for input_line in client_input.lines() {
        let input_line: String = input_line.context("cannot read from the MCP client")?;
        if input_line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = reply_to(lessons_home, &input_line) {
            // serde_json escapes newlines inside strings, so one reply is always one line.
            writeln!(mut_client_output, "{reply}").context("cannot write to the MCP client")?;
            mut_client_output.flush()?;
        }
    }
    journal!("mcp: client closed stdin, exiting");
    Ok(())
}

/// The reply to one input line, or None when none is due (a notification, or a response to a
/// request this server never sends).
fn reply_to(lessons_home: &Path, input_line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(input_line) {
        Ok(message) => message,
        Err(parse_error) => {
            return Some(error_reply(
                Value::Null,
                PARSE_ERROR,
                &format!("invalid JSON: {parse_error}"),
            ));
        }
    };
    let method: Option<&str> = message.get("method").and_then(Value::as_str);
    match (message.is_object(), method, message.get("id").cloned()) {
        (false, _, _) => Some(error_reply(
            Value::Null,
            INVALID_REQUEST,
            "expected one JSON-RPC object per line; batches are not supported",
        )),
        (true, None, _) => None,
        (true, Some(method), None) => {
            journal!("mcp: notification {method}");
            None
        }
        (true, Some(method), Some(request_id)) => {
            journal!("mcp: request {method}");
            Some(
                match handle_request(lessons_home, method, message.get("params")) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": request_id, "result": result }),
                    Err(request_error) => {
                        error_reply(request_id, request_error.code, &request_error.message)
                    }
                },
            )
        }
    }
}

fn error_reply(request_id: Value, error_code: i64, error_message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": request_id, "error": { "code": error_code, "message": error_message } })
}

fn handle_request(
    lessons_home: &Path,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, RequestError> {
    match method {
        "initialize" => Ok(initialize_result(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(lessons_home, params),
        unknown_method => Err(RequestError {
            code: METHOD_NOT_FOUND,
            message: format!("unknown method: {unknown_method}"),
        }),
    }
}

fn initialize_result(params: Option<&Value>) -> Value {
    let requested_version: Option<&str> = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str);
    let negotiated_version: &str = requested_version
        .filter(|requested| SUPPORTED_PROTOCOL_VERSIONS.contains(requested))
        .or_else(|| SUPPORTED_PROTOCOL_VERSIONS.first().copied())
        .unwrap_or_default(); // the list is a non-empty constant
    journal!(
        "mcp: client asked for protocol {requested_version:?}, answering {negotiated_version}"
    );
    json!({
        "protocolVersion": negotiated_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "lessons", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The two tools. Descriptions stay short: they are read into every agent session.
fn tool_definitions() -> Value {
    let domain_names: Vec<&str> = Domain::ALL.iter().map(|domain| domain.as_str()).collect();
    json!([
        {
            "name": "search_lessons",
            "description": "Search past lessons (problem, cause, fix, failed attempts) by keywords or pasted error text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "domains": { "type": "array", "items": { "enum": domain_names } },
                    "limit": { "type": "integer", "minimum": 1 },
                    "raw": { "type": "boolean", "description": "query is FTS5 syntax" }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "record_lesson",
            "description": "Record a lesson, only after the user approves its text. Returns its id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "domain": { "enum": domain_names },
                    "title": { "type": "string" },
                    "problem": { "type": "string", "description": "exact error text when there is one" },
                    "root_cause": { "type": "string" },
                    "resolution": { "type": "string" },
                    "failed_tries": { "type": "string", "description": "what did not work" },
                    "tags": { "type": "string", "description": "space separated" },
                    "env_versions": { "type": "string" },
                    "context": { "type": "string" }
                },
                "required": ["domain", "title", "problem"],
                "additionalProperties": false
            }
        }
    ])
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    domains: Option<Vec<Domain>>,
    limit: Option<usize>,
    #[serde(default)]
    raw: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArguments {
    domain: Domain,
    title: String,
    problem: String,
    context: Option<String>,
    root_cause: Option<String>,
    resolution: Option<String>,
    failed_tries: Option<String>,
    tags: Option<String>,
    env_versions: Option<String>,
}

/// Malformed calls (unknown tool, arguments that do not fit the schema) are protocol errors; a
/// well-formed call that fails while running (a refused domain, a missing file) is a tool result
/// with `isError`, so the agent sees the reason and can react.
fn call_tool(lessons_home: &Path, params: Option<&Value>) -> Result<Value, RequestError> {
    let fn_invalid_params = |message: String| RequestError {
        code: INVALID_PARAMS,
        message,
    };
    let tool_call: ToolCall = serde_json::from_value(params.cloned().unwrap_or(Value::Null))
        .map_err(|parse_error| {
            fn_invalid_params(format!("invalid tools/call params: {parse_error}"))
        })?;
    journal!("mcp: tool {}", tool_call.name);
    let tool_outcome: Result<String> = match tool_call.name.as_str() {
        "search_lessons" => serde_json::from_value(tool_call.arguments)
            .map_err(|parse_error| {
                fn_invalid_params(format!("invalid search_lessons arguments: {parse_error}"))
            })
            .map(|arguments| search_lessons(lessons_home, arguments))?,
        "record_lesson" => serde_json::from_value(tool_call.arguments)
            .map_err(|parse_error| {
                fn_invalid_params(format!("invalid record_lesson arguments: {parse_error}"))
            })
            .map(|arguments| record_lesson(lessons_home, arguments))?,
        unknown_tool => return Err(fn_invalid_params(format!("Unknown tool: {unknown_tool}"))),
    };
    Ok(match tool_outcome {
        Ok(result_text) => {
            json!({ "content": [{ "type": "text", "text": result_text }], "isError": false })
        }
        Err(tool_error) => {
            journal!("mcp: tool {} failed: {tool_error:#}", tool_call.name);
            json!({ "content": [{ "type": "text", "text": format!("{tool_error:#}") }], "isError": true })
        }
    })
}

/// Refuses any domain not listed in `[mcp] domains`, so an agent never reads or writes a file the
/// user kept out of agent sessions (personal.db by default).
fn refuse_unlisted(requested_domains: &[Domain], allowed_domains: &[Domain]) -> Result<()> {
    match requested_domains
        .iter()
        .find(|domain| !allowed_domains.contains(domain))
    {
        Some(blocked_domain) => bail!(
            "domain {} is not enabled for MCP; add it to [mcp] domains in $LESSONS_HOME/config.toml",
            blocked_domain.as_str()
        ),
        None => Ok(()),
    }
}

/// Returns the hits as compact JSON, the same shape as `lessons search --format json`.
fn search_lessons(lessons_home: &Path, arguments: SearchArguments) -> Result<String> {
    let mcp_config = config::load(lessons_home)?.mcp;
    let domains: Vec<Domain> = arguments
        .domains
        .unwrap_or_else(|| mcp_config.domains.clone());
    refuse_unlisted(&domains, &mcp_config.domains)?;
    let fts_query: Option<String> = if arguments.raw {
        Some(arguments.query)
    } else {
        query::fts_query_from_text(&arguments.query)
    };
    let hits: Vec<SearchHit> = match &fts_query {
        Some(fts_query) => store::search_all(
            &db::open_readable_domains(lessons_home, &domains),
            fts_query,
            arguments.limit.unwrap_or(mcp_config.max_results),
        )?,
        None => Vec::new(), // only stopwords or punctuation: nothing to look for
    };
    Ok(serde_json::to_string(&hits)?)
}

/// Stores one lesson and returns `{"id": ..., "domain": ...}`.
fn record_lesson(lessons_home: &Path, arguments: RecordArguments) -> Result<String> {
    refuse_unlisted(
        &[arguments.domain],
        &config::load(lessons_home)?.mcp.domains,
    )?;
    let conn = db::open_target(lessons_home, arguments.domain)?;
    let lesson_id: String = store::insert(
        &conn,
        &NewLesson {
            title: arguments.title,
            problem: arguments.problem,
            context: arguments.context,
            root_cause: arguments.root_cause,
            resolution: arguments.resolution,
            failed_tries: arguments.failed_tries,
            tags: arguments.tags,
            env_versions: arguments.env_versions,
        },
    )?;
    Ok(json!({ "id": lesson_id, "domain": arguments.domain.as_str() }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::OpenMode;

    /// A home with work.db and shared.db, as on a work laptop; personal.db is absent.
    fn work_laptop_home() -> tempfile::TempDir {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        [Domain::Work, Domain::Shared].iter().for_each(|domain| {
            db::open(&domain.file_path(temp_dir.path()), OpenMode::Create).expect("create");
        });
        temp_dir
    }

    /// Sends `messages`, one per line, and returns every reply in order.
    fn exchange(lessons_home: &Path, messages: &[Value]) -> Vec<Value> {
        let client_input: String = messages
            .iter()
            .map(|message| format!("{message}\n"))
            .collect();
        let mut mut_server_output: Vec<u8> = Vec::new();
        serve(
            lessons_home,
            client_input.as_bytes(),
            &mut mut_server_output,
        )
        .expect("serve");
        String::from_utf8(mut_server_output)
            .expect("UTF-8 output")
            .lines()
            .map(|reply_line| {
                serde_json::from_str(reply_line).expect("each reply line is one JSON message")
            })
            .collect()
    }

    fn tool_call(request_id: &str, tool_name: &str, arguments: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": request_id, "method": "tools/call", "params": { "name": tool_name, "arguments": arguments } })
    }

    fn tool_text(reply: &Value) -> &str {
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
    }

    #[test]
    fn initialize_echoes_a_supported_version_and_falls_back_otherwise() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[
                json!({ "jsonrpc": "2.0", "id": "known", "method": "initialize", "params": { "protocolVersion": "2025-03-26" } }),
                json!({ "jsonrpc": "2.0", "id": "future", "method": "initialize", "params": { "protocolVersion": "2099-01-01" } }),
            ],
        );
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            replies[1]["result"]["protocolVersion"],
            SUPPORTED_PROTOCOL_VERSIONS[0]
        );
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "lessons");
        assert!(replies[0]["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_reply_and_ping_gets_an_empty_result() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "id": "ping", "method": "ping" }),
            ],
        );
        assert_eq!(
            replies,
            vec![json!({ "jsonrpc": "2.0", "id": "ping", "result": {} })]
        );
    }

    #[test]
    fn tools_list_offers_exactly_the_two_tools() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[json!({ "jsonrpc": "2.0", "id": "list", "method": "tools/list" })],
        );
        let tool_names: Vec<&str> = replies[0]["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(tool_names, vec!["search_lessons", "record_lesson"]);
    }

    #[test]
    fn a_recorded_lesson_is_found_by_search() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[
                tool_call(
                    "record",
                    "record_lesson",
                    json!({ "domain": "shared", "title": "registry token expired", "problem": "pull_image: not_found_or_no_access", "failed_tries": "retrying the pull" }),
                ),
                tool_call(
                    "search",
                    "search_lessons",
                    json!({ "query": "not_found_or_no_access" }),
                ),
            ],
        );
        assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
        let recorded: Value =
            serde_json::from_str(tool_text(&replies[0])).expect("record result is JSON");
        let hits: Vec<Value> =
            serde_json::from_str(tool_text(&replies[1])).expect("search result is JSON");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0]["id"], recorded["id"]);
        assert_eq!(hits[0]["domain"], "shared");
        assert_eq!(hits[0]["failed_tries"], "retrying the pull");
    }

    #[test]
    fn personal_is_refused_for_search_and_record_and_its_file_is_never_created() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[
                tool_call(
                    "record",
                    "record_lesson",
                    json!({ "domain": "personal", "title": "t", "problem": "p" }),
                ),
                tool_call(
                    "search",
                    "search_lessons",
                    json!({ "query": "anything", "domains": ["personal"] }),
                ),
            ],
        );
        replies.iter().for_each(|reply| {
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            assert!(tool_text(reply).contains("[mcp] domains"), "{reply}");
        });
        assert!(!Domain::Personal.file_path(lessons_home.path()).exists());
    }

    #[test]
    fn protocol_errors_carry_the_json_rpc_codes() {
        let lessons_home = work_laptop_home();
        let client_input: String = [
            "this is not json".to_string(),
            json!([{ "jsonrpc": "2.0", "id": "batched", "method": "ping" }]).to_string(),
            json!({ "jsonrpc": "2.0", "id": "method", "method": "resources/list" }).to_string(),
            tool_call("tool", "delete_everything", json!({})).to_string(),
            tool_call("arguments", "record_lesson", json!({ "domain": "shared" })).to_string(),
            tool_call(
                "extra",
                "search_lessons",
                json!({ "query": "q", "unexpected": true }),
            )
            .to_string(),
        ]
        .join("\n");
        let mut mut_server_output: Vec<u8> = Vec::new();
        serve(
            lessons_home.path(),
            client_input.as_bytes(),
            &mut mut_server_output,
        )
        .expect("serve");
        let error_codes: Vec<i64> = String::from_utf8(mut_server_output)
            .expect("UTF-8 output")
            .lines()
            .map(|reply_line| {
                serde_json::from_str::<Value>(reply_line).expect("JSON reply")["error"]["code"]
                    .as_i64()
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            error_codes,
            vec![
                PARSE_ERROR,
                INVALID_REQUEST,
                METHOD_NOT_FOUND,
                INVALID_PARAMS,
                INVALID_PARAMS,
                INVALID_PARAMS
            ]
        );
    }

    #[test]
    fn a_search_with_nothing_searchable_returns_an_empty_list() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[tool_call(
                "search",
                "search_lessons",
                json!({ "query": "the a of" }),
            )],
        );
        assert_eq!(tool_text(&replies[0]), "[]");
    }
}
