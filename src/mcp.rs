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
    let mut mut_client_input = client_input;
    loop {
        let reply: Option<Value> = match read_message_line(&mut mut_client_input)? {
            InputLine::EndOfInput => break,
            InputLine::Text(input_line) if input_line.trim().is_empty() => None,
            InputLine::Text(input_line) => reply_to(lessons_home, &input_line),
            // One bad line must not end the session: answer it and read the next one.
            InputLine::NotUtf8 => Some(error_reply(
                Value::Null,
                PARSE_ERROR,
                "message is not valid UTF-8",
            )),
            InputLine::TooLong => Some(error_reply(
                Value::Null,
                PARSE_ERROR,
                &format!("message longer than {MAX_MESSAGE_BYTES} bytes"),
            )),
        };
        if let Some(reply) = reply {
            // serde_json escapes newlines inside strings, so one reply is always one line.
            writeln!(mut_client_output, "{reply}").context("cannot write to the MCP client")?;
            mut_client_output.flush()?;
        }
    }
    journal!("mcp: client closed stdin, exiting");
    Ok(())
}

/// The longest message accepted. Lesson text is small; the cap stops a runaway or hostile client
/// from making the server buffer an endless line.
const MAX_MESSAGE_BYTES: usize = 1 << 20; // 1 MiB

enum InputLine {
    Text(String),
    NotUtf8,
    TooLong,
    EndOfInput,
}

/// Reads one newline-terminated message, reading at most MAX_MESSAGE_BYTES of it into memory.
fn read_message_line(client_input: &mut impl BufRead) -> Result<InputLine> {
    let mut mut_line_bytes: Vec<u8> = Vec::new();
    let bytes_read: usize = std::io::Read::take(&mut *client_input, MAX_MESSAGE_BYTES as u64 + 1) // one byte past the cap shows the line is too long
        .read_until(b'\n', &mut mut_line_bytes)
        .context("cannot read from the MCP client")?;
    let is_too_long: bool = mut_line_bytes.len() > MAX_MESSAGE_BYTES;
    match (bytes_read, is_too_long) {
        (0, _) => Ok(InputLine::EndOfInput),
        (_, true) => client_input
            .skip_until(b'\n') // drop the rest of the oversized line
            .map(|_skipped_bytes| InputLine::TooLong)
            .context("cannot read from the MCP client"),
        (_, false) => {
            Ok(String::from_utf8(mut_line_bytes).map_or(InputLine::NotUtf8, InputLine::Text))
        }
    }
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
        // A response (it carries result or error) to a request this server never sends: nothing to do.
        (true, None, _) if message.get("result").is_some() || message.get("error").is_some() => {
            None
        }
        // An id with no method is a malformed request; the client is waiting for an answer.
        (true, None, Some(request_id)) => Some(error_reply(
            request_id,
            INVALID_REQUEST,
            "request has no method",
        )),
        (true, None, None) => None,
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
        "initialize" => initialize_result(params),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(lessons_home, params),
        unknown_method => Err(RequestError {
            code: METHOD_NOT_FOUND,
            message: format!("unknown method: {unknown_method}"),
        }),
    }
}

fn initialize_result(params: Option<&Value>) -> Result<Value, RequestError> {
    let requested_version: &str = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .ok_or_else(|| RequestError {
            code: INVALID_PARAMS,
            message: "initialize needs a string params.protocolVersion".to_string(),
        })?;
    let negotiated_version: &str = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|supported| **supported == requested_version)
        .or_else(|| SUPPORTED_PROTOCOL_VERSIONS.first())
        .copied()
        .unwrap_or_default(); // the list is a non-empty constant
    journal!("mcp: client asked for protocol {requested_version}, answering {negotiated_version}");
    Ok(json!({
        "protocolVersion": negotiated_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "lessons", "version": env!("CARGO_PKG_VERSION") },
    }))
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
            .and_then(|arguments: SearchArguments| match arguments.limit {
                Some(0) => Err(fn_invalid_params(
                    "search_lessons limit must be at least 1".to_string(),
                )),
                Some(_) | None => Ok(arguments),
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
            // Strict open: a file that exists but cannot be read is an error naming it, not an empty
            // result the agent would take for "no lessons exist".
            &db::open_existing_domains(lessons_home, &domains)?,
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

    /// Sends raw bytes (which need not be JSON or even UTF-8) and returns every reply in order.
    fn exchange_bytes(lessons_home: &Path, client_input: &[u8]) -> Vec<Value> {
        let mut mut_server_output: Vec<u8> = Vec::new();
        serve(lessons_home, client_input, &mut mut_server_output).expect("serve");
        String::from_utf8(mut_server_output)
            .expect("UTF-8 output")
            .lines()
            .map(|reply_line| serde_json::from_str(reply_line).expect("JSON reply"))
            .collect()
    }

    #[test]
    fn an_id_without_a_method_is_answered_but_a_response_is_not() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[
                json!({ "jsonrpc": "2.0", "id": "orphan" }),
                json!({ "jsonrpc": "2.0", "id": "answer", "result": {} }),
            ],
        );
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert_eq!(replies[0]["id"], "orphan");
        assert_eq!(replies[0]["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn a_bad_line_is_answered_and_the_session_goes_on() {
        let lessons_home = work_laptop_home();
        let ping_line: String = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": "after", "method": "ping" })
        );
        let oversized_line: Vec<u8> = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let client_input: Vec<u8> = [
            b"\xff\xfe not utf-8\n".as_slice(),
            ping_line.as_bytes(),
            &oversized_line,
            b"\n",
            ping_line.as_bytes(),
        ]
        .concat();
        let replies: Vec<Value> = exchange_bytes(lessons_home.path(), &client_input);
        let reply_summary: Vec<(Value, Value)> = replies
            .iter()
            .map(|reply| (reply["id"].clone(), reply["error"]["code"].clone()))
            .collect();
        assert_eq!(
            reply_summary,
            vec![
                (Value::Null, json!(PARSE_ERROR)),
                (json!("after"), Value::Null),
                (Value::Null, json!(PARSE_ERROR)),
                (json!("after"), Value::Null),
            ]
        );
    }

    #[test]
    fn an_unreadable_file_is_reported_not_taken_for_an_empty_result() {
        let lessons_home = work_laptop_home();
        std::fs::write(
            Domain::Shared.file_path(lessons_home.path()),
            "this is not a SQLite file at all",
        )
        .unwrap();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[tool_call(
                "search",
                "search_lessons",
                json!({ "query": "registry" }),
            )],
        );
        assert_eq!(replies[0]["result"]["isError"], true, "{}", replies[0]);
        assert!(
            tool_text(&replies[0]).contains("shared.db"),
            "{}",
            replies[0]
        );
    }

    #[test]
    fn a_zero_limit_and_an_initialize_without_a_version_are_invalid_params() {
        let lessons_home = work_laptop_home();
        let replies: Vec<Value> = exchange(
            lessons_home.path(),
            &[
                tool_call(
                    "zero",
                    "search_lessons",
                    json!({ "query": "registry", "limit": 0 }),
                ),
                json!({ "jsonrpc": "2.0", "id": "versionless", "method": "initialize", "params": {} }),
            ],
        );
        replies
            .iter()
            .for_each(|reply| assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{reply}"));
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
