//! Streaming response parsing for every answer provider.
//!
//! Providers speak three wire formats: Server-Sent Events from the OpenAI
//! Responses API (also used by the ChatGPT subscription backend), SSE from the
//! Anthropic Messages API, and newline-delimited JSON from Claude Code. Each
//! is reduced to the same small set of [`StreamEvent`]s.

use std::{collections::HashMap, io::BufRead};

use agentdictate_core::AnswerSource;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StreamEvent {
    Text(String),
    Searching(String),
    Source(AnswerSource),
    Done,
    Error(String),
}

/// One dispatched Server-Sent Event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SseMessage {
    pub event: Option<String>,
    pub data: String,
}

/// Reads SSE messages from a byte stream. Returns `None` at end of stream.
pub(crate) fn next_sse_message(reader: &mut impl BufRead) -> std::io::Result<Option<SseMessage>> {
    let mut message = SseMessage::default();
    let mut has_data = false;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(has_data.then_some(message));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if has_data {
                return Ok(Some(message));
            }
            message.event = None;
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        let (field, value) = trimmed
            .split_once(':')
            .map_or((trimmed, ""), |(field, value)| {
                (field, value.strip_prefix(' ').unwrap_or(value))
            });
        match field {
            "event" => message.event = Some(value.to_owned()),
            "data" => {
                if has_data {
                    message.data.push('\n');
                }
                message.data.push_str(value);
                has_data = true;
            }
            _ => {}
        }
    }
}

fn error_message(value: &Value) -> String {
    value
        .pointer("/error/message")
        .or_else(|| value.pointer("/response/error/message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| "The model returned an error.".to_owned(), str::to_owned)
}

fn search_query(item: &Value) -> Option<String> {
    item.pointer("/action/query")
        .and_then(Value::as_str)
        .or_else(|| item.pointer("/action/queries/0").and_then(Value::as_str))
        .map(str::to_owned)
}

/// Maps one OpenAI Responses stream event.
pub(crate) fn parse_responses_event(value: &Value) -> Vec<StreamEvent> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "response.output_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .filter(|delta| !delta.is_empty())
            .map(|delta| vec![StreamEvent::Text(delta.to_owned())])
            .unwrap_or_default(),
        "response.output_item.added"
            if value.pointer("/item/type").and_then(Value::as_str) == Some("web_search_call") =>
        {
            vec![StreamEvent::Searching("Searching the web".to_owned())]
        }
        "response.output_item.done"
            if value.pointer("/item/type").and_then(Value::as_str) == Some("web_search_call") =>
        {
            value
                .get("item")
                .and_then(search_query)
                .map(|query| vec![StreamEvent::Searching(query)])
                .unwrap_or_default()
        }
        "response.output_text.annotation.added" => {
            let annotation = value.get("annotation").unwrap_or(&Value::Null);
            match annotation.get("url").and_then(Value::as_str) {
                Some(url) => vec![StreamEvent::Source(AnswerSource {
                    title: annotation
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .unwrap_or(url)
                        .to_owned(),
                    url: url.to_owned(),
                })],
                None => Vec::new(),
            }
        }
        "response.completed" => vec![StreamEvent::Done],
        "response.incomplete" => vec![StreamEvent::Done],
        "response.failed" | "error" => vec![StreamEvent::Error(error_message(value))],
        _ => Vec::new(),
    }
}

/// Stateful mapper for Anthropic Messages stream events.
#[derive(Default)]
pub(crate) struct AnthropicStream {
    tool_inputs: HashMap<u64, String>,
}

impl AnthropicStream {
    pub(crate) fn parse(&mut self, value: &Value) -> Vec<StreamEvent> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        match kind {
            "content_block_start" => {
                let block = value.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("server_tool_use") | Some("tool_use") => {
                        self.tool_inputs.insert(index, String::new());
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if name.eq_ignore_ascii_case("web_search") || name == "WebSearch" {
                            vec![StreamEvent::Searching("Searching the web".to_owned())]
                        } else if name.eq_ignore_ascii_case("web_fetch") || name == "WebFetch" {
                            vec![StreamEvent::Searching("Reading a page".to_owned())]
                        } else {
                            Vec::new()
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let delta = value.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| vec![StreamEvent::Text(text.to_owned())])
                        .unwrap_or_default(),
                    Some("input_json_delta") => {
                        if let (Some(input), Some(partial)) = (
                            self.tool_inputs.get_mut(&index),
                            delta.get("partial_json").and_then(Value::as_str),
                        ) {
                            input.push_str(partial);
                        }
                        Vec::new()
                    }
                    Some("citations_delta") => {
                        let citation = delta.get("citation").unwrap_or(&Value::Null);
                        citation
                            .get("url")
                            .and_then(Value::as_str)
                            .map(|url| {
                                vec![StreamEvent::Source(AnswerSource {
                                    title: citation
                                        .get("title")
                                        .and_then(Value::as_str)
                                        .filter(|title| !title.is_empty())
                                        .unwrap_or(url)
                                        .to_owned(),
                                    url: url.to_owned(),
                                })]
                            })
                            .unwrap_or_default()
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => self
                .tool_inputs
                .remove(&index)
                .and_then(|input| serde_json::from_str::<Value>(&input).ok())
                .and_then(|input| {
                    input
                        .get("query")
                        .or_else(|| input.get("url"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .map(|query| vec![StreamEvent::Searching(query)])
                .unwrap_or_default(),
            "message_delta" => match value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                Some("refusal") => vec![StreamEvent::Error(
                    "The model declined to answer this request.".to_owned(),
                )],
                _ => Vec::new(),
            },
            "message_stop" => vec![StreamEvent::Done],
            "error" => vec![StreamEvent::Error(error_message(value))],
            _ => Vec::new(),
        }
    }
}

/// Stateful mapper for `claude -p --output-format stream-json` lines.
#[derive(Default)]
pub(crate) struct ClaudeCodeStream {
    inner: AnthropicStream,
}

impl ClaudeCodeStream {
    pub(crate) fn parse(&mut self, value: &Value) -> Vec<StreamEvent> {
        match value.get("type").and_then(Value::as_str) {
            Some("stream_event") => value
                .get("event")
                .map(|event| {
                    self.inner
                        .parse(event)
                        .into_iter()
                        // The CLI reports the turn's end with a result line.
                        .filter(|event| *event != StreamEvent::Done)
                        .collect()
                })
                .unwrap_or_default(),
            Some("assistant") => value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_use")
                        })
                        .filter_map(|block| {
                            block
                                .pointer("/input/query")
                                .or_else(|| block.pointer("/input/url"))
                                .and_then(Value::as_str)
                                .map(|query| StreamEvent::Searching(query.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Some("result") => {
                if value.get("is_error").and_then(Value::as_bool) == Some(true) {
                    let message = value
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or("Claude Code reported an error.");
                    vec![StreamEvent::Error(claude_code_error(message))]
                } else {
                    vec![StreamEvent::Done]
                }
            }
            _ => Vec::new(),
        }
    }
}

fn claude_code_error(message: &str) -> String {
    let lower = message.to_lowercase();
    if lower.contains("authenticate") || lower.contains("oauth") || lower.contains("log in") {
        "Claude Code is signed out. Run `claude auth login` in a terminal, then try again."
            .to_owned()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn sse_messages_join_data_lines_and_skip_comments() {
        let mut reader =
            Cursor::new(": keepalive\n\nevent: delta\ndata: {\"a\":\ndata: 1}\n\ndata: [DONE]\n\n");
        let first = next_sse_message(&mut reader).unwrap().unwrap();
        assert_eq!(first.event.as_deref(), Some("delta"));
        assert_eq!(first.data, "{\"a\":\n1}");
        let second = next_sse_message(&mut reader).unwrap().unwrap();
        assert_eq!(second.event, None);
        assert_eq!(second.data, "[DONE]");
        assert_eq!(next_sse_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn responses_events_map_text_search_sources_and_completion() {
        assert_eq!(
            parse_responses_event(&json!({"type":"response.output_text.delta","delta":"Hi"})),
            vec![StreamEvent::Text("Hi".into())]
        );
        assert_eq!(
            parse_responses_event(
                &json!({"type":"response.output_item.done","item":{"type":"web_search_call","action":{"type":"search","query":"rust 1.99"}}})
            ),
            vec![StreamEvent::Searching("rust 1.99".into())]
        );
        assert_eq!(
            parse_responses_event(
                &json!({"type":"response.output_item.done","item":{"type":"web_search_call","action":{"queries":["first"]}}})
            ),
            vec![StreamEvent::Searching("first".into())]
        );
        assert_eq!(
            parse_responses_event(
                &json!({"type":"response.output_text.annotation.added","annotation":{"type":"url_citation","url":"https://blog.rust-lang.org","title":"Rust Blog"}})
            ),
            vec![StreamEvent::Source(AnswerSource {
                title: "Rust Blog".into(),
                url: "https://blog.rust-lang.org".into()
            })]
        );
        assert_eq!(
            parse_responses_event(&json!({"type":"response.completed"})),
            vec![StreamEvent::Done]
        );
        assert_eq!(
            parse_responses_event(
                &json!({"type":"response.failed","response":{"error":{"message":"bad model"}}})
            ),
            vec![StreamEvent::Error("bad model".into())]
        );
    }

    #[test]
    fn anthropic_events_report_search_queries_after_input_streams() {
        let mut stream = AnthropicStream::default();
        assert_eq!(
            stream.parse(&json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","name":"web_search"}})),
            vec![StreamEvent::Searching("Searching the web".into())]
        );
        stream.parse(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"opus"}}));
        stream.parse(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":" 5.5\"}"}}));
        assert_eq!(
            stream.parse(&json!({"type":"content_block_stop","index":0})),
            vec![StreamEvent::Searching("opus 5.5".into())]
        );
        assert_eq!(
            stream.parse(&json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Answer"}})),
            vec![StreamEvent::Text("Answer".into())]
        );
        assert_eq!(
            stream.parse(&json!({"type":"content_block_delta","index":2,"delta":{"type":"citations_delta","citation":{"url":"https://a.example","title":""}}})),
            vec![StreamEvent::Source(AnswerSource { title: "https://a.example".into(), url: "https://a.example".into() })]
        );
        assert_eq!(
            stream.parse(&json!({"type":"message_delta","delta":{"stop_reason":"refusal"}})),
            vec![StreamEvent::Error(
                "The model declined to answer this request.".into()
            )]
        );
        assert_eq!(
            stream.parse(&json!({"type":"message_stop"})),
            vec![StreamEvent::Done]
        );
    }

    #[test]
    fn claude_code_lines_stream_text_and_explain_sign_in_failures() {
        let mut stream = ClaudeCodeStream::default();
        assert_eq!(
            stream.parse(&json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}})),
            vec![StreamEvent::Text("Hello".into())]
        );
        assert!(
            stream
                .parse(&json!({"type":"stream_event","event":{"type":"message_stop"}}))
                .is_empty()
        );
        assert_eq!(
            stream.parse(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"WebSearch","input":{"query":"latest news"}}]}})),
            vec![StreamEvent::Searching("latest news".into())]
        );
        assert_eq!(
            stream.parse(&json!({"type":"result","is_error":false,"result":"Hello"})),
            vec![StreamEvent::Done]
        );
        let failure = stream.parse(&json!({"type":"result","is_error":true,"result":"Failed to authenticate: OAuth session expired"}));
        assert!(
            matches!(&failure[0], StreamEvent::Error(message) if message.contains("claude auth login"))
        );
    }
}
