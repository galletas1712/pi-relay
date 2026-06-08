use std::collections::BTreeSet;
use std::fmt::Write as _;

use agent_store::{InputPriority, WorkflowVariable, WorkflowVariableWrite};
use agent_vocab::UserMessage;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::codec::from_params;
use crate::state::AppState;
use crate::subagents::send_to_subagent;
use crate::types::RpcError;

const MAX_TEMPLATE_BYTES: usize = 64 * 1024;
const MAX_VARIABLE_BYTES: usize = 256 * 1024;
const DEFAULT_VARIABLE_LIST_LIMIT: i64 = 100;
const MAX_VARIABLE_LIST_LIMIT: i64 = 200;

pub(crate) async fn workflow_vars_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: WorkflowScopedParams = from_params(params)?;
    let session_id = validate_identifier(params.session_id, "session_id")?;
    let owner_session_id = workflow_owner_session_id(state, &session_id).await?;
    let workflow_id = validate_identifier(params.workflow_id, "workflow_id")?;
    let limit = bounded_limit(params.limit);
    let variables = state
        .repo
        .list_workflow_variables(&owner_session_id, &workflow_id, limit)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "owner_session_id": owner_session_id,
        "workflow_id": workflow_id,
        "limit": limit,
        "variables": variables.into_iter().map(variable_summary_view).collect::<Vec<_>>(),
    }))
}

pub(crate) async fn workflow_context_send_tool(
    state: &AppState,
    parent_session_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    params.insert("parent_session_id".to_string(), json!(parent_session_id));
    workflow_context_send(state, Value::Object(params)).await
}

pub(crate) async fn workflow_var_write_tool(
    state: &AppState,
    session_id: &str,
    action_row_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    params.insert("session_id".to_string(), json!(session_id));
    workflow_var_write_with_producer(
        state,
        Value::Object(params),
        Some(ProducerOverride {
            session_id,
            action_row_id,
        }),
    )
    .await
}

pub(crate) async fn workflow_var_read_tool(
    state: &AppState,
    session_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    params.insert("session_id".to_string(), json!(session_id));
    workflow_var_read(state, Value::Object(params)).await
}

pub(crate) async fn workflow_vars_list_tool(
    state: &AppState,
    session_id: &str,
    args: Value,
) -> std::result::Result<Value, RpcError> {
    let mut params = object_args(args)?;
    params.insert("session_id".to_string(), json!(session_id));
    workflow_vars_list(state, Value::Object(params)).await
}

pub(crate) async fn workflow_var_read(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let params: WorkflowVarReadParams = from_params(params)?;
    let session_id = validate_identifier(params.session_id, "session_id")?;
    let owner_session_id = workflow_owner_session_id(state, &session_id).await?;
    let workflow_id = validate_identifier(params.workflow_id, "workflow_id")?;
    let name = validate_identifier(params.name, "name")?;
    let variable = state
        .repo
        .workflow_variable(&owner_session_id, &workflow_id, &name)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            RpcError::new("variable_not_found", format!("variable not found: {name}"))
        })?;
    Ok(variable_view(variable))
}

pub(crate) async fn workflow_var_write(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    workflow_var_write_with_producer(state, params, None).await
}

async fn workflow_var_write_with_producer(
    state: &AppState,
    params: Value,
    producer: Option<ProducerOverride<'_>>,
) -> std::result::Result<Value, RpcError> {
    let params: WorkflowVarWriteParams = from_params(params)?;
    let session_id = validate_identifier(params.session_id.clone(), "session_id")?;
    let owner_session_id = workflow_owner_session_id(state, &session_id).await?;
    let request = WorkflowVarWriteRequest::from_params(params, owner_session_id, producer)?;
    let variable = state
        .repo
        .write_workflow_variable(&WorkflowVariableWrite {
            owner_session_id: request.owner_session_id,
            workflow_id: request.workflow_id,
            name: request.name,
            value_json: request.value_json,
            value_text: request.value_text,
            producer_session_id: request.producer_session_id,
            producer_action_id: request.producer_action_id,
        })
        .await
        .map_err(anyhow::Error::from)?;
    Ok(variable_view(variable))
}

pub(crate) async fn workflow_context_send(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let request = WorkflowContextSendRequest::from_params(params)?;
    let owner_session_id = workflow_owner_session_id(state, &request.parent_session_id).await?;
    let variable_names = template_variable_names(&request.template)?;
    let mut variables = Vec::with_capacity(variable_names.len());
    for name in variable_names {
        let variable = state
            .repo
            .workflow_variable(&owner_session_id, &request.workflow_id, &name)
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| {
                RpcError::new("variable_not_found", format!("variable not found: {name}"))
            })?;
        variables.push(variable);
    }
    let rendered = render_template(&request.template, &variables)?;
    let send = send_to_subagent(
        state,
        &request.parent_session_id,
        &request.child_session_id,
        request.priority,
        UserMessage::text(rendered.clone()),
        request.client_input_id,
    )
    .await?;
    Ok(json!({
        "owner_session_id": owner_session_id,
        "workflow_id": request.workflow_id,
        "parent_session_id": request.parent_session_id,
        "child_session_id": request.child_session_id,
        "rendered": rendered,
        "send": send,
    }))
}

#[derive(Debug, Deserialize)]
struct WorkflowScopedParams {
    session_id: String,
    workflow_id: String,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WorkflowVarReadParams {
    session_id: String,
    workflow_id: String,
    name: String,
}

#[derive(Debug)]
struct WorkflowVarWriteRequest {
    owner_session_id: String,
    workflow_id: String,
    name: String,
    value_json: Option<Value>,
    value_text: Option<String>,
    producer_session_id: Option<String>,
    producer_action_id: Option<String>,
}

struct ProducerOverride<'a> {
    session_id: &'a str,
    action_row_id: &'a str,
}

impl WorkflowVarWriteRequest {
    fn from_params(
        params: WorkflowVarWriteParams,
        owner_session_id: String,
        producer: Option<ProducerOverride<'_>>,
    ) -> std::result::Result<Self, RpcError> {
        let workflow_id = validate_identifier(params.workflow_id, "workflow_id")?;
        let name = validate_identifier(params.name, "name")?;
        if params.value_json.is_none() && params.value_text.is_none() {
            return Err(RpcError::new(
                "invalid_params",
                "value_json or value_text is required",
            ));
        }
        ensure_variable_value_size(params.value_json.as_ref(), params.value_text.as_deref())?;
        Ok(Self {
            owner_session_id,
            workflow_id,
            name,
            value_json: params.value_json,
            value_text: params.value_text,
            producer_session_id: producer
                .as_ref()
                .map(|producer| producer.session_id.to_string()),
            producer_action_id: producer
                .as_ref()
                .map(|producer| producer.action_row_id.to_string()),
        })
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowVarWriteParams {
    session_id: String,
    workflow_id: String,
    name: String,
    value_json: Option<Value>,
    value_text: Option<String>,
}

struct WorkflowContextSendRequest {
    workflow_id: String,
    parent_session_id: String,
    child_session_id: String,
    template: String,
    priority: InputPriority,
    client_input_id: Option<String>,
}

impl WorkflowContextSendRequest {
    fn from_params(params: Value) -> std::result::Result<Self, RpcError> {
        let params: WorkflowContextSendParams = from_params(params)?;
        let workflow_id = validate_identifier(params.workflow_id, "workflow_id")?;
        let parent_session_id = validate_identifier(params.parent_session_id, "parent_session_id")?;
        let child_session_id = validate_identifier(params.child_session_id, "child_session_id")?;
        let template = params.template.trim().to_string();
        if template.is_empty() {
            return Err(RpcError::new("invalid_params", "template cannot be empty"));
        }
        if template.len() > MAX_TEMPLATE_BYTES {
            return Err(RpcError::new(
                "template_too_large",
                format!("template exceeds {MAX_TEMPLATE_BYTES} bytes"),
            ));
        }
        Ok(Self {
            workflow_id,
            parent_session_id,
            child_session_id,
            template,
            priority: params.priority.unwrap_or(InputPriority::FollowUp),
            client_input_id: params.client_input_id,
        })
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowContextSendParams {
    workflow_id: String,
    parent_session_id: String,
    child_session_id: String,
    template: String,
    priority: Option<InputPriority>,
    client_input_id: Option<String>,
}

fn validate_identifier(value: String, field: &str) -> std::result::Result<String, RpcError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(RpcError::new(
            "invalid_params",
            format!("{field} cannot be empty"),
        ));
    }
    Ok(value)
}

async fn workflow_owner_session_id(
    state: &AppState,
    session_id: &str,
) -> std::result::Result<String, RpcError> {
    if !state
        .repo
        .session_exists(session_id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Err(RpcError::new("session_not_found", "session not found"));
    }
    let relationship = state
        .repo
        .session_relationship_for_target(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(relationship
        .map(|relationship| relationship.root_session_id)
        .unwrap_or_else(|| session_id.to_string()))
}

fn bounded_limit(limit: Option<u32>) -> i64 {
    limit
        .map(i64::from)
        .unwrap_or(DEFAULT_VARIABLE_LIST_LIMIT)
        .clamp(1, MAX_VARIABLE_LIST_LIMIT)
}

fn object_args(value: Value) -> std::result::Result<serde_json::Map<String, Value>, RpcError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(RpcError::new(
            "invalid_params",
            "tool arguments must be a JSON object",
        )),
    }
}

fn ensure_variable_value_size(
    value_json: Option<&Value>,
    value_text: Option<&str>,
) -> std::result::Result<(), RpcError> {
    let json_bytes = value_json
        .map(|value| serde_json::to_vec(value).map(|bytes| bytes.len()))
        .transpose()
        .map_err(|error| RpcError::new("invalid_params", error.to_string()))?
        .unwrap_or_default();
    let text_bytes = value_text.map(str::len).unwrap_or_default();
    let bytes = json_bytes.saturating_add(text_bytes);
    if bytes > MAX_VARIABLE_BYTES {
        return Err(RpcError::new(
            "variable_too_large",
            format!("variable value exceeds {MAX_VARIABLE_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn variable_view(variable: WorkflowVariable) -> Value {
    json!({
        "owner_session_id": variable.owner_session_id,
        "workflow_id": variable.workflow_id,
        "name": variable.name,
        "value_json": variable.value_json,
        "value_text": variable.value_text,
        "producer_session_id": variable.producer_session_id,
        "producer_action_id": variable.producer_action_id,
        "created_at": variable.created_at,
        "updated_at": variable.updated_at,
    })
}

fn variable_summary_view(variable: WorkflowVariable) -> Value {
    let value_json_bytes = variable
        .value_json
        .as_ref()
        .and_then(|value| serde_json::to_vec(value).ok().map(|bytes| bytes.len()));
    let value_text_bytes = variable.value_text.as_ref().map(String::len);
    json!({
        "owner_session_id": variable.owner_session_id,
        "workflow_id": variable.workflow_id,
        "name": variable.name,
        "has_value_json": variable.value_json.is_some(),
        "has_value_text": variable.value_text.is_some(),
        "value_json_bytes": value_json_bytes,
        "value_text_bytes": value_text_bytes,
        "producer_session_id": variable.producer_session_id,
        "producer_action_id": variable.producer_action_id,
        "created_at": variable.created_at,
        "updated_at": variable.updated_at,
    })
}

fn template_variable_names(template: &str) -> std::result::Result<Vec<String>, RpcError> {
    let mut names = BTreeSet::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            return Err(RpcError::new(
                "invalid_template",
                "template contains an unclosed variable",
            ));
        };
        let name = rest[..close].trim();
        if name.is_empty() {
            return Err(RpcError::new(
                "invalid_template",
                "template contains an empty variable name",
            ));
        }
        names.insert(name.to_string());
        rest = &rest[close + 1..];
    }
    Ok(names.into_iter().collect())
}

fn render_template(
    template: &str,
    variables: &[WorkflowVariable],
) -> std::result::Result<String, RpcError> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            return Err(RpcError::new(
                "invalid_template",
                "template contains an unclosed variable",
            ));
        };
        let name = rest[..close].trim();
        if name.is_empty() {
            return Err(RpcError::new(
                "invalid_template",
                "template contains an empty variable name",
            ));
        }
        let Some(variable) = variables.iter().find(|variable| variable.name == name) else {
            return Err(RpcError::new(
                "variable_not_found",
                format!("variable not found: {name}"),
            ));
        };
        write!(&mut rendered, "{}", variable_template_value(variable))
            .map_err(|error| RpcError::new("template_error", error.to_string()))?;
        if rendered.len() > MAX_TEMPLATE_BYTES {
            return Err(RpcError::new(
                "rendered_template_too_large",
                format!("rendered template exceeds {MAX_TEMPLATE_BYTES} bytes"),
            ));
        }
        rest = &rest[close + 1..];
    }
    rendered.push_str(rest);
    if rendered.len() > MAX_TEMPLATE_BYTES {
        return Err(RpcError::new(
            "rendered_template_too_large",
            format!("rendered template exceeds {MAX_TEMPLATE_BYTES} bytes"),
        ));
    }
    Ok(rendered)
}

fn variable_template_value(variable: &WorkflowVariable) -> String {
    if let Some(text) = &variable.value_text {
        return text.clone();
    }
    variable
        .value_json
        .as_ref()
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variable(
        name: &str,
        value_text: Option<&str>,
        value_json: Option<Value>,
    ) -> WorkflowVariable {
        WorkflowVariable {
            owner_session_id: "owner".to_string(),
            workflow_id: "workflow_1".to_string(),
            name: name.to_string(),
            value_json,
            value_text: value_text.map(str::to_string),
            producer_session_id: None,
            producer_action_id: None,
            created_at: "created".to_string(),
            updated_at: "updated".to_string(),
        }
    }

    #[test]
    fn render_template_interpolates_text_and_json_variables() {
        let rendered = render_template(
            "Review:\n{review}\nData:\n{data}",
            &[
                variable("review", Some("Looks good."), None),
                variable("data", None, Some(json!({ "ok": true }))),
            ],
        )
        .expect("template renders");
        assert!(rendered.contains("Looks good."));
        assert!(rendered.contains("\"ok\": true"));
    }

    #[test]
    fn render_template_rejects_missing_variable() {
        let error = render_template("Missing {nope}", &[]).expect_err("missing variable");
        assert_eq!(error.code, "variable_not_found");
    }

    #[test]
    fn template_variable_names_returns_unique_sorted_names() {
        let names = template_variable_names("{beta} { alpha } {beta}").expect("names parse");
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn write_request_uses_daemon_producer_override() {
        let request = WorkflowVarWriteRequest::from_params(
            WorkflowVarWriteParams {
                session_id: "session".to_string(),
                workflow_id: "workflow_1".to_string(),
                name: "result".to_string(),
                value_json: None,
                value_text: Some("done".to_string()),
            },
            "owner".to_string(),
            Some(ProducerOverride {
                session_id: "producer",
                action_row_id: "action",
            }),
        )
        .expect("request parses");

        assert_eq!(request.owner_session_id, "owner");
        assert_eq!(request.producer_session_id.as_deref(), Some("producer"));
        assert_eq!(request.producer_action_id.as_deref(), Some("action"));
    }

    #[test]
    fn write_request_requires_a_value() {
        let error = WorkflowVarWriteRequest::from_params(
            WorkflowVarWriteParams {
                session_id: "session".to_string(),
                workflow_id: "workflow_1".to_string(),
                name: "result".to_string(),
                value_json: None,
                value_text: None,
            },
            "owner".to_string(),
            None,
        )
        .expect_err("value is required");
        assert_eq!(error.code, "invalid_params");
    }

    #[test]
    fn write_request_rejects_large_values() {
        let error = WorkflowVarWriteRequest::from_params(
            WorkflowVarWriteParams {
                session_id: "session".to_string(),
                workflow_id: "workflow_1".to_string(),
                name: "result".to_string(),
                value_json: None,
                value_text: Some("x".repeat(MAX_VARIABLE_BYTES + 1)),
            },
            "owner".to_string(),
            None,
        )
        .expect_err("large value is rejected");
        assert_eq!(error.code, "variable_too_large");
    }
}
