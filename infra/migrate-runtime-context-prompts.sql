\set ON_ERROR_STOP on

begin;

update sessions
set system_prompt = replace(
        system_prompt,
        $old$When a task surfaces that matches one (or more) of the available skills, call `LoadSkill` for each skill you want to gain.
Each invocation of `LoadSkill` will insert useful context about the chosen domain in your context before acting, which makes you more knowledgeable!$old$,
        $new$When a task surfaces that matches an available skill, call `LoadSkill` with its exact name, then read the returned `SKILL.md` path before acting. Resolve relative links in that file from its enclosing directory.$new$
    ),
    updated_at = now()
where position(
    $old$When a task surfaces that matches one (or more) of the available skills, call `LoadSkill` for each skill you want to gain.
Each invocation of `LoadSkill` will insert useful context about the chosen domain in your context before acting, which makes you more knowledgeable!$old$
    in system_prompt
) > 0;

update sessions
set system_prompt = replace(
        system_prompt,
        $old$Activate one of the available skills by name. Use this when a task matches a skill description; pi-relay will inject that skill's instructions into the model context. If the skill is already loaded, the tool reports that it is already loaded.$old$,
        $new$Resolve an available skill name to the absolute path of its SKILL.md on the runtime host.$new$
    ),
    updated_at = now()
where position(
    $old$Activate one of the available skills by name. Use this when a task matches a skill description; pi-relay will inject that skill's instructions into the model context. If the skill is already loaded, the tool reports that it is already loaded.$old$
    in system_prompt
) > 0;

update sessions
set system_prompt = replace(
        system_prompt,
        $old$The subagent role skill name. Use prefixed workspace role names like "repo/reviewer" for workspace-scoped skills; unprefixed names resolve configured global roles such as "implementer".$old$,
        $new$The exact unprefixed runtime-global role name from the packaged subagent roles catalog, for example "implementer".$new$
    ),
    updated_at = now()
where position(
    $old$The subagent role skill name. Use prefixed workspace role names like "repo/reviewer" for workspace-scoped skills; unprefixed names resolve configured global roles such as "implementer".$old$
    in system_prompt
) > 0;

update sessions
set system_prompt = replace(
        system_prompt,
        $old$The subagent role skill name. Use prefixed workspace role names like "repo/reviewer" for workspace-scoped skills; unprefixed names resolve configured global roles such as "reviewer".$old$,
        $new$The exact unprefixed runtime-global role name from the packaged subagent roles catalog, for example "reviewer".$new$
    ),
    updated_at = now()
where position(
    $old$The subagent role skill name. Use prefixed workspace role names like "repo/reviewer" for workspace-scoped skills; unprefixed names resolve configured global roles such as "reviewer".$old$
    in system_prompt
) > 0;

commit;
