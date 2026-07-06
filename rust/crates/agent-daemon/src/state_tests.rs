use std::sync::Arc;

use agent_prompt::PromptProfile;
use agent_tools::{ProviderTool, ToolDescriptor, ToolExtension, ToolRegistry};
use agent_vocab::ProviderKind;
use serde_json::json;

use super::ProviderToolSnapshots;

struct LargeToolExtension;

impl ToolExtension for LargeToolExtension {
    fn id(&self) -> &'static str {
        "large-tool-snapshot-test"
    }

    fn register(&self, registry: &mut ToolRegistry) {
        for index in (0..256).rev() {
            let canonical_name = format!("bulk_{index:03}");
            let name = format!("tool_{:03}", 255 - index);
            registry.register_tool(ToolDescriptor::new(canonical_name).provider(
                ProviderKind::OpenAi,
                ProviderTool::function_json_named(
                    ProviderKind::OpenAi,
                    name,
                    "bulk test tool",
                    json!({ "type": "object" }),
                ),
            ));
        }
        for (canonical_name, name) in [
            ("case_lower_b", "read"),
            ("case_upper", "Read"),
            ("case_lower_a", "read"),
            ("interrupt_subagent", "interrupt_subagent"),
        ] {
            let descriptor = ToolDescriptor::new(canonical_name);
            registry.register_tool(
                descriptor
                    .provider(
                        ProviderKind::OpenAi,
                        ProviderTool::function_json_named(
                            ProviderKind::OpenAi,
                            name,
                            "ordering test tool",
                            json!({ "type": "object" }),
                        ),
                    )
                    .provider(
                        ProviderKind::Claude,
                        ProviderTool::function_json_named(
                            ProviderKind::Claude,
                            name,
                            "ordering test tool",
                            json!({ "type": "object" }),
                        ),
                    ),
            );
        }
    }
}

#[test]
fn provider_profile_tool_snapshots_are_sorted_filtered_and_shared() {
    let mut registry = ToolRegistry::new();
    registry.register_extension(&LargeToolExtension);
    let snapshots = ProviderToolSnapshots::new(&registry);

    let openai_parent = snapshots.get(ProviderKind::OpenAi, PromptProfile::Parent);
    let openai_parent_again = snapshots.get(ProviderKind::OpenAi, PromptProfile::Parent);
    let openai_subagent = snapshots.get(ProviderKind::OpenAi, PromptProfile::Subagent);
    let claude_parent = snapshots.get(ProviderKind::Claude, PromptProfile::Parent);
    let claude_subagent = snapshots.get(ProviderKind::Claude, PromptProfile::Subagent);

    assert!(Arc::ptr_eq(&openai_parent, &openai_parent_again));
    assert!(!Arc::ptr_eq(&openai_parent, &openai_subagent));
    assert!(!Arc::ptr_eq(&claude_parent, &claude_subagent));
    assert_ne!(openai_parent.as_ptr(), claude_parent.as_ptr());

    let openai_tie_order = openai_parent
        .iter()
        .filter(|tool| tool.name.eq_ignore_ascii_case("read"))
        .map(|tool| (tool.name.as_str(), tool.canonical_name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        openai_tie_order,
        [
            ("Read", "case_upper"),
            ("read", "case_lower_a"),
            ("read", "case_lower_b"),
        ]
    );
    assert_eq!(
        claude_parent
            .iter()
            .map(|tool| (tool.name.as_str(), tool.canonical_name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("interrupt_subagent", "interrupt_subagent"),
            ("Read", "case_upper"),
            ("read", "case_lower_a"),
            ("read", "case_lower_b"),
        ]
    );
    assert!(openai_parent
        .iter()
        .any(|tool| tool.canonical_name == "interrupt_subagent"));
    assert!(!openai_subagent
        .iter()
        .any(|tool| tool.canonical_name == "interrupt_subagent"));
    assert!(!claude_subagent
        .iter()
        .any(|tool| tool.canonical_name == "interrupt_subagent"));

    for _ in 0..1_000 {
        assert!(Arc::ptr_eq(
            &openai_parent,
            &snapshots.get(ProviderKind::OpenAi, PromptProfile::Parent)
        ));
    }
}
