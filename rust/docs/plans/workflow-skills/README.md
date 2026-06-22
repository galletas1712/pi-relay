# Draft workflow skills (install in Phase 4)

These are **drafts**, not live skills. They are intentionally kept out of any
directory the skill loader scans (`subagent-roles/` and workspace skill dirs) so
that running agents do not see guidance for `stage.*` tools that do not exist
yet.

Install them in **Phase 4** of `../workflow-orchestration.md`:

1. Add a global workflow-skill directory to the loader. In
   `rust/crates/agent-daemon/src/provider_runtime/skills.rs`, alongside the
   existing `load_global_skills_from_dir(&prompt_root.join("subagent-roles"))`,
   add `load_global_skills_from_dir(&prompt_root.join("workflows"))`.
2. Copy these files to `workflows/<name>/SKILL.md` at the prompt root.
3. They then appear in the skills index and are loaded with `LoadSkill`, exactly
   like the `subagent-roles/*` skills.

Naming: workflow skills are prefixed `workflow-*` so they never collide with the
subagent **role** skills of the same topic (e.g. the `explore` role vs the
`workflow-explore` pattern).

Each skill documents a parent-interpreted, possibly-cyclic **stage state
machine**. The parent drives it with `stage.start_full` /
`stage.start_readonly_fanout`, branching on the typed `suggested_next` outcomes
that appear in each stage's handoff `index.json`. See "Workflows are skills, not
a DSL" in the spec.
