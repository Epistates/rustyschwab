//! Calling a server in process, as a client with no elicitation channel
//! would on the current protocol revision: for this crate's tests, its
//! integration tests and the evaluation harness. Not a stable API.
use crate::contract::ToolResult;
use serde_json::Value;
use turbomcp::prelude::*;

/// A `tools/call` context with no client channel.
pub fn call_context() -> CallToolContext {
    CallToolContext::new(RequestContext::default())
}

/// Call tool `name` on `server` with `arguments` (an object).
pub async fn call_tool<S: WithTools>(
    server: &S,
    name: &str,
    arguments: Value,
) -> McpResult<ToolResult> {
    let arguments = match arguments {
        Value::Object(arguments) => arguments,
        _ => serde_json::Map::new(),
    };
    server
        .call_tool(
            &call_context(),
            neutral::CallToolParams::new(name, arguments),
        )
        .await
}

/// Every tool `server` lists, first page only (none of ours paginate).
pub async fn tools<S: WithTools>(server: &S) -> Vec<neutral::Tool> {
    let ctx = ListToolsContext::new(RequestContext::default());
    server
        .list_tools(&ctx, neutral::ListParams::new())
        .await
        .map(|page| page.tools)
        .unwrap_or_default()
}

/// Every resource `server` lists.
pub async fn resources<S: WithResources>(server: &S) -> Vec<neutral::Resource> {
    let ctx = ListResourcesContext::new(RequestContext::default());
    server
        .list_resources(&ctx, neutral::ListParams::new())
        .await
        .map(|page| page.resources)
        .unwrap_or_default()
}

/// Every prompt `server` lists.
pub async fn prompts<S: WithPrompts>(server: &S) -> Vec<neutral::Prompt> {
    let ctx = ListPromptsContext::new(RequestContext::default());
    server
        .list_prompts(&ctx, neutral::ListParams::new())
        .await
        .map(|page| page.prompts)
        .unwrap_or_default()
}

/// Read resource `uri` from `server` and return its text.
pub async fn read_text<S: WithResources>(server: &S, uri: &str) -> McpResult<String> {
    let ctx = ReadResourceContext::new(RequestContext::default());
    let result = server
        .read_resource(&ctx, neutral::ReadResourceParams::new(uri))
        .await?;
    result
        .contents
        .into_iter()
        .find_map(|contents| match contents {
            neutral::ResourceContents::Text { text, .. } => Some(text),
            _ => None,
        })
        .ok_or_else(|| McpError::internal("resource has no text"))
}

/// Render prompt `name` from `server` with `arguments` (string values) and
/// return its first message's text.
pub async fn prompt_text<S: WithPrompts>(
    server: &S,
    name: &str,
    arguments: Option<Value>,
) -> McpResult<String> {
    let arguments = arguments
        .and_then(|arguments| serde_json::from_value(arguments).ok())
        .unwrap_or_default();
    let ctx = GetPromptContext::new(RequestContext::default());
    let result = server
        .get_prompt(&ctx, neutral::GetPromptParams::new(name, arguments))
        .await?;
    match result.messages.into_iter().next().map(|m| m.content) {
        Some(neutral::Content::Text { text, .. }) => Ok(text),
        _ => Err(McpError::internal("prompt has no text")),
    }
}

/// What the model reads from `result`: the parsed success text, or the
/// structured failure.
pub fn reading(result: &ToolResult) -> Value {
    if result.is_error {
        return result.structured_content.clone().unwrap_or(Value::Null);
    }
    crate::contract::first_text(result)
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}
