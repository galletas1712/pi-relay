use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use agent_tools::ProviderTool;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;

/// A dynamically rendered provider body whose tool declarations remain
/// borrowed until the final serialization.
#[derive(Debug)]
pub(crate) struct ProviderRequestBody {
    body: Value,
    tools: ToolDeclarations,
}

#[derive(Debug)]
enum ToolDeclarations {
    None,
    Empty,
    Shared(Arc<[ProviderTool]>),
}

impl ProviderRequestBody {
    pub(crate) fn without_tools(body: Value) -> Self {
        Self {
            body,
            tools: ToolDeclarations::None,
        }
    }

    pub(crate) fn with_empty_tools(body: Value) -> Self {
        Self {
            body,
            tools: ToolDeclarations::Empty,
        }
    }

    pub(crate) fn with_tools(body: Value, tools: Arc<[ProviderTool]>) -> Self {
        Self {
            body,
            tools: ToolDeclarations::Shared(tools),
        }
    }

    pub(crate) fn into_body_without_tools(self) -> Value {
        debug_assert!(matches!(self.tools, ToolDeclarations::None));
        self.body
    }

    #[cfg(test)]
    pub(crate) fn materialize(&self) -> serde_json::Result<Value> {
        serde_json::to_value(self)
    }
}

impl Deref for ProviderRequestBody {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

impl DerefMut for ProviderRequestBody {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.body
    }
}

impl Serialize for ProviderRequestBody {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let tools = match &self.tools {
            ToolDeclarations::None => return self.body.serialize(serializer),
            ToolDeclarations::Empty => &[][..],
            ToolDeclarations::Shared(tools) => tools.as_ref(),
        };
        let Value::Object(body) = &self.body else {
            return self.body.serialize(serializer);
        };

        let mut map = serializer.serialize_map(Some(body.len()))?;
        for (key, value) in body {
            if key == "tools" {
                map.serialize_entry(key, &BorrowedToolDeclarations(tools))?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

struct BorrowedToolDeclarations<'a>(&'a [ProviderTool]);

impl Serialize for BorrowedToolDeclarations<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for tool in self.0 {
            sequence.serialize_element(&BorrowedDeclaration(&tool.declaration))?;
        }
        sequence.end()
    }
}

struct BorrowedDeclaration<'a>(&'a Value);

impl Serialize for BorrowedDeclaration<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[cfg(test)]
        record_declaration_references(self.0);
        self.0.serialize(serializer)
    }
}

#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SerializedDeclarationReferences {
    pub(crate) declarations: Vec<*const Value>,
    pub(crate) maps: Vec<*const serde_json::Map<String, Value>>,
    pub(crate) strings: Vec<*const u8>,
}

#[cfg(test)]
thread_local! {
    static SERIALIZED_DECLARATION_REFERENCES:
        std::cell::RefCell<Option<SerializedDeclarationReferences>> = const {
            std::cell::RefCell::new(None)
        };
}

#[cfg(test)]
fn record_declaration_references(declaration: &Value) {
    fn record_strings(value: &Value, references: &mut SerializedDeclarationReferences) {
        match value {
            Value::String(value) => references.strings.push(value.as_ptr()),
            Value::Array(values) => {
                for value in values {
                    record_strings(value, references);
                }
            }
            Value::Object(values) => {
                references.maps.push(std::ptr::from_ref(values));
                for (key, value) in values {
                    references.strings.push(key.as_ptr());
                    record_strings(value, references);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    SERIALIZED_DECLARATION_REFERENCES.with(|references| {
        let mut references = references.borrow_mut();
        if let Some(references) = references.as_mut() {
            references
                .declarations
                .push(std::ptr::from_ref(declaration));
            record_strings(declaration, references);
        }
    });
}

#[cfg(test)]
pub(crate) fn observe_serialized_declaration_references<T>(
    operation: impl FnOnce() -> T,
) -> (T, SerializedDeclarationReferences) {
    SERIALIZED_DECLARATION_REFERENCES.with(|references| {
        assert!(references.borrow().is_none());
        *references.borrow_mut() = Some(SerializedDeclarationReferences::default());
    });
    let result = operation();
    let references = SERIALIZED_DECLARATION_REFERENCES.with(|references| {
        references
            .borrow_mut()
            .take()
            .expect("declaration observation was installed")
    });
    (result, references)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_tools::{ProviderTool, ToolExecution};
    use serde_json::{json, Value};

    use super::{observe_serialized_declaration_references, ProviderRequestBody};

    #[test]
    fn serialization_visits_original_declaration_and_string_allocations() {
        let tools: Arc<[ProviderTool]> = vec![ProviderTool::new(
            "read",
            "read a file",
            json!({ "type": "object" }),
            json!({
                "name": "read",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    }
                }
            }),
            ToolExecution::LocalJson,
        )]
        .into();
        let declaration = std::ptr::from_ref(&tools[0].declaration);
        let declaration_map = std::ptr::from_ref(
            tools[0]
                .declaration
                .as_object()
                .expect("tool declaration object"),
        );
        let nested_string = tools[0].declaration["input_schema"]["properties"]["path"]["type"]
            .as_str()
            .expect("nested declaration string")
            .as_ptr();
        let body = ProviderRequestBody::with_tools(
            json!({
                "model": "test",
                "tools": null,
            }),
            Arc::clone(&tools),
        );

        let (serialized, references) =
            observe_serialized_declaration_references(|| serde_json::to_vec(&body));

        assert_eq!(
            serde_json::from_slice::<Value>(&serialized.expect("body serializes"))
                .expect("body is JSON"),
            json!({
                "model": "test",
                "tools": [tools[0].declaration],
            })
        );
        assert_eq!(references.declarations, vec![declaration]);
        assert!(references.maps.contains(&declaration_map));
        assert!(references.strings.contains(&nested_string));
    }
}
