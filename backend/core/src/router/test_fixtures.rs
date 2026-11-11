//! Shared independently authored Chat wire fixtures for codec and adapter tests.

use serde_json::{Value, json};

/// Version marker for the shared Chat fixture set.
pub(super) const CHAT_FIXTURE_VERSION: &str = "chat-v1";

/// Independently authored OpenAI Chat request wire JSON.
pub(crate) fn chat_request() -> Value {
    json!({
        "model": "fixture-route",
        "messages": [
            {
                "role": "system",
                "content": "You are a file assistant."
            },
            {
                "role": "user"
            },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_read_0",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    }
                ],
                "name": "assistant"
            },
            {
                "role": "tool",
                "tool_call_id": "call_read_0",
                "content": "README contents"
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a UTF-8 file.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string"
                            }
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    },
                    "strict": false
                }
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": {
                "name": "read_file"
            }
        },
        "parallel_tool_calls": false,
        "stream": false,
        "stream_options": {
            "include_usage": true
        },
        "temperature": 0.25,
        "top_p": 0.75,
        "max_tokens": 64,
        "max_completion_tokens": 32,
        "stop": ["\nObservation:", "\nFinal:"]
    })
}

/// Independently authored OpenAI Responses request wire JSON.
pub(super) fn responses_request_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "fixture-route",
        "store": false,
        "instructions": "You are a file assistant.",
        "input": [
            {"type": "message", "role": "user", "content": "Read the fixture."},
            {"type": "function_call", "call_id": "call_read_0", "name": "read_file", "arguments": "{\"path\":\"README.md\"}"},
            {"type": "function_call_output", "call_id": "call_read_0", "output": "README contents"},
            {"type": "message", "role": "user", "content": [{"type":"input_text","text":"Summarize it."}]}
        ],
        "tools": [{
            "type": "function",
            "name": "read_file",
            "description": "Read a UTF-8 file.",
            "parameters": {"type":"object"},
            "strict": false
        }],
        "tool_choice": {"type":"function", "name":"read_file"},
        "parallel_tool_calls": true,
        "stream": true,
        "max_output_tokens": 64,
        "temperature": 0.25,
        "top_p": 0.75
    }))
    .expect("Responses fixture JSON")
}

/// A Codex Responses request after an MCP tool call and a final answer, each preceded by
/// reasoning Codex replays. Trimmed from a real Codex 0.167 request: shorter texts, IDs and tool
/// lists, with an echo MCP server.
pub(super) fn codex_responses_request_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "route",
        "stream": true,
        "input": [
            {"type": "message", "id": "msg_1", "role": "developer", "content": [{"type": "input_text", "text": "You are a coding agent running in the Codex CLI."}]},
            {"type": "message", "id": "msg_2", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>\n  <cwd>/work</cwd>\n</environment_context>"}]},
            {"type": "message", "id": "msg_3", "role": "user", "content": [{"type": "input_text", "text": "echo ping"}]},
            {"type": "reasoning", "id": "rs_1", "summary": [], "content": [{"type": "reasoning_text", "text": "I should echo."}], "encrypted_content": null},
            {"type": "function_call", "status": "completed", "id": "fc_1", "name": "echo", "namespace": "mcp__echo", "arguments": "{\"text\":\"ping\"}", "call_id": "call_1"},
            {"type": "function_call_output", "id": "fco_1", "call_id": "call_1", "output": [{"type": "input_text", "text": "Wall time: 0.0025 seconds\nOutput:"}, {"type": "input_text", "text": "ping"}]},
            {"type": "reasoning", "id": "rs_2", "summary": [], "content": [{"type": "reasoning_text", "text": "The echo answered."}], "encrypted_content": null},
            {"type": "message", "status": "completed", "id": "msg_4", "role": "assistant", "content": [{"type": "output_text", "text": "pong", "annotations": []}]},
            {"type": "message", "id": "msg_5", "role": "user", "content": [{"type": "input_text", "text": "again"}]}
        ],
        "tools": [
            {
                "type": "function",
                "name": "exec_command",
                "description": "Runs a command in a PTY.",
                "strict": false,
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"], "additionalProperties": false}
            },
            {
                "type": "namespace",
                "name": "multi_agent_v1",
                "description": "Tools for spawning and managing sub-agents.",
                "tools": [{
                    "type": "function",
                    "name": "spawn_agent",
                    "description": "Spawn a sub-agent.",
                    "strict": false,
                    "parameters": {"type": "object", "properties": {"message": {"type": "string"}}, "additionalProperties": false}
                }]
            },
            {
                "type": "namespace",
                "name": "mcp__echo",
                "description": "Tools in the mcp__echo namespace.",
                "tools": [{
                    "type": "function",
                    "name": "echo",
                    "description": "Echoes the given text back.",
                    "strict": false,
                    "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
                }]
            }
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"summary": "auto"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "thread-1",
        "client_metadata": {"thread_id": "thread-1", "turn_id": "turn-1"}
    }))
    .expect("Codex Responses fixture JSON")
}

/// Independently authored expected llama.cpp request wire JSON oracle.
pub(crate) fn llama_request_oracle() -> Value {
    json!({
        "model": "fixture-route",
        "messages": [
            {
                "role": "system",
                "content": "You are a file assistant."
            },
            {
                "role": "user"
            },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_read_0",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    }
                ],
                "name": "assistant"
            },
            {
                "role": "tool",
                "content": "README contents",
                "tool_call_id": "call_read_0"
            }
        ],
        "stream": false,
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a UTF-8 file.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string"
                            }
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    },
                    "strict": false
                }
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": {
                "name": "read_file"
            }
        },
        "parallel_tool_calls": false,
        "temperature": 0.25,
        "top_p": 0.75,
        "max_tokens": 64,
        "max_completion_tokens": 32,
        "stop": ["\nObservation:", "\nFinal:"]
    })
}

/// Independently authored expected completion output wire JSON oracle.
pub(super) fn completion() -> Value {
    json!({
        "id": "chatcmpl_fixture",
        "object": "chat.completion",
        "created": 1720000000,
        "model": "fixture-route",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_read_1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"src/main.rs\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }
        ],
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 7,
            "total_tokens": 28
        }
    })
}

/// Independently authored expected stream wire JSON frames, ending with `[DONE]`.
pub(super) fn stream_frames() -> Value {
    json!([
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": []
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "I will read that."
                    },
                    "finish_reason": null
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {},
                    "finish_reason": null
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "content": null
                    },
                    "finish_reason": null
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_stream_0",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": "{\"pa"
                                }
                            }
                        ]
                    },
                    "finish_reason": null
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "function": {
                                    "arguments": "th"
                                }
                            }
                        ]
                    },
                    "finish_reason": null
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "function": {
                                    "arguments": "}"
                                }
                            }
                        ]
                    },
                    "finish_reason": null
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [
                {
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }
            ]
        },
        {
            "id": "chatcmpl_stream_fixture",
            "object": "chat.completion.chunk",
            "created": 1720000001,
            "model": "fixture-route",
            "choices": [],
            "usage": {
                "prompt_tokens": 21,
                "completion_tokens": 7,
                "total_tokens": 28
            }
        },
        "[DONE]"
    ])
}

/// Expected SSE fragmentation sizes for the shared stream frames.
pub(super) fn stream_fragment_sizes() -> Vec<usize> {
    vec![
        17, 3, 41, 2, 97, 11, 5, 113, 7, 31, 19, 53, 29, 89, 13, 43, 23, 71, 37, 61,
    ]
}

/// OpenAI Chat SSE bytes reconstructed through the fixture fragmentation plan.
pub(crate) fn fragmented_stream_body() -> Vec<u8> {
    let mut body = Vec::new();
    for frame in stream_frames().as_array().expect("stream frames") {
        if let Some(done) = frame.as_str() {
            body.extend_from_slice(format!("data: {done}\n\n").as_bytes());
        } else {
            body.extend_from_slice(b"data: ");
            body.extend_from_slice(
                serde_json::to_string(frame)
                    .expect("stream frame")
                    .as_bytes(),
            );
            body.extend_from_slice(b"\n\n");
        }
    }
    let mut fragments = Vec::new();
    let mut offset = 0;
    for size in stream_fragment_sizes() {
        if offset == body.len() {
            break;
        }
        let end = (offset + size).min(body.len());
        fragments.push(body[offset..end].to_vec());
        offset = end;
    }
    if offset < body.len() {
        fragments.push(body[offset..].to_vec());
    }
    assert!(fragments.len() > 1, "fixture must use fragmented input");
    fragments.concat()
}

/// Invalid upstream SSE body used by rejection tests.
pub(crate) fn invalid_sse() -> &'static str {
    "data: {\"choices\":[}\n\n"
}

/// Pi-shaped streamed Chat request for a reasoning model served by ninfer, with the intended
/// sampling, effort, and template controls. It is raw wire text so every control keeps its
/// exact JSON spelling.
pub(crate) const PI_CHAT_REQUEST: &str = r##"{"model":"qwen3-8-27b","messages":[{"role":"developer","content":"You are Pi."},{"role":"user","content":"Read the README."},{"role":"assistant","content":null,"reasoning":"The file is needed.","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"# Readme"}],"tools":[{"type":"function","function":{"name":"read","description":"Read a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},"strict":false}}],"stream":true,"stream_options":{"include_usage":true},"store":false,"max_completion_tokens":32000,"reasoning_effort":"none","temperature":1.0,"top_p":0.95,"presence_penalty":0.0,"frequency_penalty":0,"top_k":20,"repetition_penalty":1.0,"min_p":0,"seed":18446744073709551615,"chat_template_kwargs":{"enable_thinking":false,"preserve_thinking":true,"reasoning_effort":"low","stops":[null,"</think>"]}}"##;

/// Engine controls of [`PI_CHAT_REQUEST`] as canonical extension text, in key order.
pub(crate) const PI_CHAT_EXTENSIONS: &str = r#"{"chat_template_kwargs":{"enable_thinking":false,"preserve_thinking":true,"reasoning_effort":"low","stops":[null,"</think>"]},"min_p":0,"repetition_penalty":1.0,"seed":18446744073709551615,"top_k":20}"#;

/// Engine controls whose numbers no 64-bit integer or double holds exactly, in key order.
pub(crate) const EXACT_NUMBER_EXTENSIONS: &str = r#"{"above_2_53":9007199254740993,"above_u64":184467440737095516160,"float":0.1000000000000000055511151231257827,"i64_min":-9223372036854775808,"u64_max":18446744073709551615}"#;

/// Text stream shaped like a captured ninfer response: ordinary chunks carry `usage: null`
/// and a final choice-less chunk carries the real usage.
pub(crate) const NINFER_TEXT_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1759262400,\"model\":\"qwen3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1759262400,\"model\":\"qwen3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1759262400,\"model\":\"qwen3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1759262400,\"model\":\"qwen3.8-27b\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":2,\"total_tokens\":14}}\n\n",
    "data: [DONE]\n\n",
);

/// Stream with separate reasoning, a function call whose identity and arguments arrive in
/// fragments with null placeholders, and final usage with cached and reasoning details.
pub(crate) const REASONING_TOOL_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me \"},\"logprobs\":null,\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"check.\",\"content\":null},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"type\":null,\"function\":{\"name\":null,\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":12,\"total_tokens\":42,\"prompt_tokens_details\":{\"cached_tokens\":16},\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n",
    "data: [DONE]\n\n",
);
