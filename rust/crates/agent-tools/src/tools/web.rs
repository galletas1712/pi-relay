use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};
use async_trait::async_trait;
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;

use crate::context::ToolContext;
use crate::error::{ToolError, ToolResult};
use crate::output::limit_tool_output_with_max_tokens;
use crate::registry::AgentTool;

#[derive(Debug, Clone, Copy)]
pub struct WebSearchTool;

#[derive(Debug, Clone, Copy)]
pub struct WebFetchTool;

#[derive(Debug, Deserialize)]
pub struct WebSearchArgs {
    pub query: String,
    #[serde(default)]
    pub recency: Option<String>,
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

async fn read_bounded_response_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_WEB_FETCH_BODY_BYTES as u64)
    {
        return Err(format!(
            "response body exceeds {MAX_WEB_FETCH_BODY_BYTES} bytes"
        ));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_WEB_FETCH_BODY_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("request body read error: {error}"))?
    {
        append_bounded_chunk(&mut body, &chunk, MAX_WEB_FETCH_BODY_BYTES)?;
    }
    Ok(body)
}

fn append_bounded_chunk(body: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<(), String> {
    if body.len().saturating_add(chunk.len()) > limit {
        return Err(format!("response body exceeds {limit} bytes"));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub fn nonempty_domains(domains: Option<&[String]>) -> Option<Vec<String>> {
    let domains = domains?
        .iter()
        .map(|domain| domain.trim())
        .filter(|domain| !domain.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!domains.is_empty()).then_some(domains)
}

const WEB_FETCH_USER_AGENT: &str = "pi-relay-web-fetch/0.1";
const MAX_WEB_FETCH_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct WebFetchArgs {
    pub url: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

#[async_trait]
impl AgentTool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "web_search",
            "Search the web for current information and return concise, cited results.".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The web search query."
                    },
                    "recency": {
                        "type": "string",
                        "description": "Optional recency filter such as day, week, month, or year."
                    },
                    "allowed_domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional allow-list of domains to include in results."
                    },
                    "blocked_domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional block-list of domains to exclude from results."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Maximum approximate tokens to return. Defaults to the tool output cap."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: WebSearchArgs = serde_json::from_str(&call.args_json)?;
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "web_search query cannot be empty".to_string(),
            ));
        }
        let mut output = format!(
            "web_search is registered as a local tool, but no web search backend is configured in this daemon. Query: {}",
            args.query.trim()
        );
        if let Some(recency) = args
            .recency
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            output.push_str(&format!("\nRecency filter: {recency}"));
        }
        if let Some(domains) = nonempty_domains(args.allowed_domains.as_deref()) {
            output.push_str(&format!("\nAllowed domains: {}", domains.join(", ")));
        }
        if let Some(domains) = nonempty_domains(args.blocked_domains.as_deref()) {
            output.push_str(&format!("\nBlocked domains: {}", domains.join(", ")));
        }
        Ok(ToolResultMessage::error(
            call.id.clone(),
            &call.tool_name,
            limit_tool_output_with_max_tokens(output, args.max_output_tokens),
        ))
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "web_fetch",
            "Fetch a web page by URL and return bounded text content. This local tool is provider-neutral; pi-relay executes the fetch outside the main model turn."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http(s) URL to fetch."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Optional note describing what the caller wants to extract or summarize from the fetched content."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Maximum approximate tokens to return. Defaults to the tool output cap."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> ToolResult<ToolResultMessage> {
        let args: WebFetchArgs = serde_json::from_str(&call.args_json)?;
        let url = validate_http_url(&args.url)?;
        Ok(fetch_url(call, _ctx, args, url).await)
    }
}

async fn fetch_url(
    call: &ToolCall,
    ctx: &ToolContext,
    args: WebFetchArgs,
    url: Url,
) -> ToolResultMessage {
    let client = match reqwest::Client::builder()
        .user_agent(WEB_FETCH_USER_AGENT)
        .timeout(ctx.timeout)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("web_fetch failed to initialize HTTP client: {error}"),
            )
        }
    };

    let response = match client.get(url.clone()).send().await {
        Ok(response) => response,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("web_fetch request failed for {url}: {error}"),
            )
        }
    };

    let status = response.status();
    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let bytes = match read_bounded_response_body(response).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ToolResultMessage::error(
                call.id.clone(),
                &call.tool_name,
                format!("web_fetch failed to read response body for {final_url}: {error}"),
            )
        }
    };

    let body = String::from_utf8_lossy(&bytes);
    let content = if looks_like_html(&content_type, &body) {
        html_to_text(&body)
    } else {
        body.to_string()
    };

    let mut output = format!(
        "URL: {url}\nFinal URL: {final_url}\nStatus: {} {}\nContent-Type: {content_type}\nBytes: {}\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        bytes.len()
    );
    if let Some(prompt) = args
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        output.push_str(&format!("Prompt: {prompt}\n"));
    }
    output.push_str("\nContent:\n");
    output.push_str(content.trim());

    let output = limit_tool_output_with_max_tokens(output, args.max_output_tokens);
    if status.is_success() {
        ToolResultMessage::success(call.id.clone(), &call.tool_name, output)
    } else {
        ToolResultMessage::error(call.id.clone(), &call.tool_name, output)
    }
}

fn validate_http_url(url: &str) -> ToolResult<Url> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(ToolError::InvalidInput(
            "web_fetch url cannot be empty".to_string(),
        ));
    }
    let parsed = Url::parse(trimmed)
        .map_err(|error| ToolError::InvalidInput(format!("web_fetch url is invalid: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::InvalidInput(
            "web_fetch url must start with http:// or https://".to_string(),
        ));
    }
    Ok(parsed)
}

fn looks_like_html(content_type: &str, body: &str) -> bool {
    content_type.to_ascii_lowercase().contains("html")
        || body
            .get(..body.len().min(512))
            .is_some_and(|head| head.to_ascii_lowercase().contains("<html"))
}

fn html_to_text(html: &str) -> String {
    let without_scripts = remove_tag_section(html, "script");
    let without_styles = remove_tag_section(&without_scripts, "style");
    collapse_whitespace(&decode_basic_entities(&strip_tags(&without_styles)))
}

fn remove_tag_section(input: &str, tag: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    loop {
        let lower = remaining.to_ascii_lowercase();
        let Some(start) = lower.find(&open) else {
            output.push_str(remaining);
            break;
        };
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start..];
        let lower_after_start = after_start.to_ascii_lowercase();
        let Some(end) = lower_after_start.find(&close) else {
            break;
        };
        remaining = &after_start[end + close.len()..];
    }
    output
}

fn strip_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => {
                in_tag = true;
                output.push(' ');
            }
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn decode_basic_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn bounded_chunk_reader_rejects_oversized_chunk_without_growing_body() {
        let mut body = Vec::new();
        append_bounded_chunk(&mut body, b"abcd", 5).expect("first chunk fits");
        let error = append_bounded_chunk(&mut body, b"xy", 5)
            .expect_err("chunked response must stop at the byte ceiling");
        assert!(error.contains("response body exceeds 5 bytes"));
        assert_eq!(body, b"abcd");
    }

    #[tokio::test]
    async fn bounded_response_reader_rejects_oversized_declared_body() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind response fixture");
        let address = listener.local_addr().expect("response fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept response request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_WEB_FETCH_BODY_BYTES + 1
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response headers");
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .expect("request response fixture");
        let error = read_bounded_response_body(response)
            .await
            .expect_err("oversized response must be rejected");
        assert!(error.contains(&format!(
            "response body exceeds {MAX_WEB_FETCH_BODY_BYTES} bytes"
        )));
        server.await.expect("response fixture server");
    }
}
