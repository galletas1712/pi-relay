#!/usr/bin/env node
/**
 * Temporary one-time migration for persisted model-facing delegation tool names.
 *
 * This script rewrites old provider/model-facing tool names in existing local
 * Postgres session data:
 *
 *   stage_start_full            -> delegate_writing_task
 *   stage_start_readonly_fanout -> delegate_readonly_tasks
 *   stage_status                -> inspect_delegation
 *   stage_cancel                -> cancel_delegation
 *
 * Runbook:
 *   1. Stop pi-agentd / the daemon first and ensure no active model/tool actions
 *      are running. This migration rewrites durable history; do not race live
 *      writers.
 *   2. Take a database backup, for example:
 *        pg_dump "$DATABASE_URL" > pi-relay-before-delegation-tool-migration.sql
 *   3. Dry-run:
 *        DATABASE_URL=postgres://... node scripts/migrate-delegation-tool-names.mjs
 *      Optionally scope a test run to exactly one session with:
 *        ... --session-id session_...
 *      Child/subagent sessions are not included; unscoped mode scans all sessions.
 *   4. Apply:
 *        DATABASE_URL=postgres://... node scripts/migrate-delegation-tool-names.mjs --apply
 *   5. Restart the daemon.
 *   6. Remove this script after the one-time local migration is no longer needed.
 *
 * Safety:
 *   - Dry-run is the default. --apply is required to mutate.
 *   - DATABASE_URL or --database-url is required.
 *   - Apply runs all row updates in one transaction.
 *   - The script only replaces exact underscore tool-name substrings. It does
 *     not target dotted web/client RPC names such as stage.start_full, internal
 *     fields such as stage_id, table names, enum statuses, or handoff filenames.
 *
 * Implementation notes:
 *   - Uses the existing `psql` CLI instead of adding a Node Postgres dependency.
 *   - Discovers candidate columns before scanning, so older local schemas that
 *     lack newer tables/columns are handled gracefully.
 *   - JSON/JSONB values are migrated recursively; string values that themselves
 *     contain parseable JSON are parsed, migrated recursively, and serialized
 *     back so provider raw JSON replay sidecars are covered.
 */

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import process from "node:process";

const TOOL_NAME_MAPPINGS = Object.freeze({
	stage_start_full: "delegate_writing_task",
	stage_start_readonly_fanout: "delegate_readonly_tasks",
	stage_status: "inspect_delegation",
	stage_cancel: "cancel_delegation",
});

const OLD_TOOL_NAMES = Object.freeze(Object.keys(TOOL_NAME_MAPPINGS));

// Durable storage surfaces from rust/crates/agent-store/src/postgres/schema.rs
// that can contain model-visible transcript/tool/prompt/session state.
//
// Intentionally not targeted:
// - projects.workspaces / sessions.workspaces / outer_cwd: workspace/path/source
//   metadata, not model-facing tool history.
// - sessions.provider_config: provider/model settings, not tool lists.
// - stages.* and sessions.stage_id: internal stage records/status/ids.
// - queued_inputs.priority/status, actions.kind/status, events.type: enums.
// - project name: display text, not prompt/tool configuration.
const CANDIDATE_COLUMNS = Object.freeze([
	{
		table: "transcript_entries",
		column: "item",
		type: "jsonb",
		keyColumns: ["session_id", "id"],
		sessionColumn: "session_id",
		reason: "durable transcript items: assistant tool calls, tool results, compaction/user text",
	},
	{
		table: "transcript_entries",
		column: "provider_replay",
		type: "jsonb",
		keyColumns: ["session_id", "id"],
		sessionColumn: "session_id",
		reason: "provider replay sidecars, including raw provider JSON and nested raw JSON strings",
	},
	{
		table: "actions",
		column: "payload",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: "session_id",
		reason: "durable tool action payloads store tool_call.tool_name/name",
	},
	{
		table: "actions",
		column: "result",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: "session_id",
		nullable: true,
		reason: "completed action results may include replayed tool call/result details",
	},
	{
		table: "events",
		column: "payload",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: "session_id",
		reason: "websocket replay events can embed transcript fragments and action payloads",
	},
	{
		table: "queued_inputs",
		column: "content",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: "session_id",
		reason: "queued/accepted user or steer messages can mention old tool names",
	},
	{
		table: "queued_inputs",
		column: "origin",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: "session_id",
		nullable: true,
		reason: "queue provenance is JSON; included for completeness if future/local builds stored text there",
	},
	{
		table: "sessions",
		column: "system_prompt",
		type: "text",
		keyColumns: ["id"],
		sessionColumn: "id",
		reason: "rendered PI.md/system prompt persisted at session creation",
	},
	{
		table: "sessions",
		column: "metadata",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: "id",
		reason: "subagent task metadata and local session prompt/config metadata",
	},
	{
		table: "projects",
		column: "metadata",
		type: "jsonb",
		keyColumns: ["id"],
		sessionColumn: null,
		reason: "project-level local JSON metadata; project workspaces/name are intentionally skipped",
	},
	{
		table: "daemon_config",
		column: "value",
		type: "jsonb",
		keyColumns: ["key"],
		sessionColumn: null,
		reason: "reserved global daemon JSON config/prompt cache; no session scope",
	},
]);

function zeroCounts() {
	return Object.fromEntries(OLD_TOOL_NAMES.map((name) => [name, 0]));
}

function addCounts(target, source) {
	for (const name of OLD_TOOL_NAMES) {
		target[name] = (target[name] ?? 0) + (source[name] ?? 0);
	}
	return target;
}

function totalCount(counts) {
	return OLD_TOOL_NAMES.reduce((sum, name) => sum + (counts[name] ?? 0), 0);
}

function replaceToolNamesInString(input) {
	let output = input;
	const counts = zeroCounts();
	for (const [oldName, newName] of Object.entries(TOOL_NAME_MAPPINGS)) {
		let index = output.indexOf(oldName);
		while (index !== -1) {
			counts[oldName] += 1;
			index = output.indexOf(oldName, index + oldName.length);
		}
		if (counts[oldName] > 0) {
			output = output.split(oldName).join(newName);
		}
	}
	return { value: output, counts, changed: output !== input };
}

function looksLikeJson(text) {
	const trimmed = text.trim();
	if (trimmed.length < 2) {
		return false;
	}
	return (
		(trimmed.startsWith("{") && trimmed.endsWith("}")) ||
		(trimmed.startsWith("[") && trimmed.endsWith("]")) ||
		(trimmed.startsWith('"') && trimmed.endsWith('"'))
	);
}

function migrateString(input, options = {}) {
	const counts = zeroCounts();
	let working = input;
	let changed = false;

	if (!options.skipNestedJson && looksLikeJson(input)) {
		try {
			const parsed = JSON.parse(input);
			const nested = migrateJsonValue(parsed);
			addCounts(counts, nested.counts);
			return {
				value: nested.changed ? JSON.stringify(nested.value) : input,
				counts,
				changed: nested.changed,
			};
		} catch {
			// Not actually JSON; fall through to plain substring replacement.
		}
	}

	const replaced = replaceToolNamesInString(working);
	addCounts(counts, replaced.counts);
	if (replaced.changed) {
		working = replaced.value;
		changed = true;
	}

	return { value: working, counts, changed };
}

function migrateObjectKey(key) {
	const replacement = TOOL_NAME_MAPPINGS[key];
	if (!replacement) {
		return { key, counts: zeroCounts(), changed: false };
	}
	const counts = zeroCounts();
	counts[key] = 1;
	return { key: replacement, counts, changed: true };
}

function migrateJsonValue(value) {
	if (typeof value === "string") {
		return migrateString(value);
	}
	if (Array.isArray(value)) {
		const counts = zeroCounts();
		let changed = false;
		const migrated = value.map((item) => {
			const result = migrateJsonValue(item);
			addCounts(counts, result.counts);
			changed = changed || result.changed;
			return result.value;
		});
		return { value: changed ? migrated : value, counts, changed };
	}
	if (value && typeof value === "object") {
		const counts = zeroCounts();
		let changed = false;
		const migrated = {};
		for (const [key, child] of Object.entries(value)) {
			const keyResult = migrateObjectKey(key);
			const childResult = migrateJsonValue(child);
			addCounts(counts, keyResult.counts);
			addCounts(counts, childResult.counts);
			changed = changed || keyResult.changed || childResult.changed;
			if (Object.hasOwn(migrated, keyResult.key)) {
				throw new Error(
					`key collision while migrating JSON object key ${JSON.stringify(key)} to ${JSON.stringify(
						keyResult.key,
					)}`,
				);
			}
			migrated[keyResult.key] = childResult.value;
		}
		return { value: changed ? migrated : value, counts, changed };
	}
	return { value, counts: zeroCounts(), changed: false };
}

function migrateTextValue(value) {
	if (value === null || value === undefined) {
		return { value, counts: zeroCounts(), changed: false };
	}
	return migrateString(String(value), { skipNestedJson: true });
}

function parseArgs(argv) {
	const options = {
		apply: false,
		databaseUrl: process.env.DATABASE_URL,
		sessionId: null,
		selfTest: false,
		help: false,
	};
	for (let i = 0; i < argv.length; i += 1) {
		const arg = argv[i];
		if (arg === "--apply") {
			options.apply = true;
		} else if (arg === "--dry-run") {
			options.apply = false;
		} else if (arg === "--self-test") {
			options.selfTest = true;
		} else if (arg === "--help" || arg === "-h") {
			options.help = true;
		} else if (arg === "--database-url") {
			i += 1;
			if (!argv[i]) {
				throw new Error("--database-url requires a value");
			}
			options.databaseUrl = argv[i];
		} else if (arg.startsWith("--database-url=")) {
			options.databaseUrl = arg.slice("--database-url=".length);
		} else if (arg === "--session-id") {
			i += 1;
			if (!argv[i]) {
				throw new Error("--session-id requires a value");
			}
			options.sessionId = argv[i];
		} else if (arg.startsWith("--session-id=")) {
			options.sessionId = arg.slice("--session-id=".length);
		} else {
			throw new Error(`unknown argument: ${arg}`);
		}
	}
	return options;
}

function printHelp() {
	console.log(`Usage:
  node scripts/migrate-delegation-tool-names.mjs [--dry-run]
  node scripts/migrate-delegation-tool-names.mjs --apply
  node scripts/migrate-delegation-tool-names.mjs --self-test

Options:
  --apply                 Mutate the database. Without this flag, dry-run only.
  --dry-run               Explicit dry-run/read-only mode (default).
  --database-url <url>    Postgres URL. Defaults to DATABASE_URL env var.
  --session-id <id>       Scope session-owned tables to exactly this session only.
                          Child/subagent sessions are not included; unscoped scans all.
                          daemon_config is global and is skipped when scoped.
  --self-test             Run migration logic tests without a database.
  --help                  Show this help.

Before --apply: stop the daemon, ensure no active actions are running, and run:
  pg_dump "$DATABASE_URL" > pi-relay-before-delegation-tool-migration.sql
`);
}

function runPsql(databaseUrl, args, input = undefined) {
	const result = spawnSync("psql", ["--no-psqlrc", "--set", "ON_ERROR_STOP=1", "--dbname", databaseUrl, ...args], {
		input,
		encoding: "utf8",
		maxBuffer: 256 * 1024 * 1024,
	});
	if (result.error) {
		if (result.error.code === "ENOENT") {
			throw new Error("psql was not found on PATH; install PostgreSQL client tools or run from an environment with psql");
		}
		throw result.error;
	}
	if (result.status !== 0) {
		throw new Error(
			`psql failed with exit code ${result.status}\nSTDOUT:\n${result.stdout}\nSTDERR:\n${result.stderr}`,
		);
	}
	return result.stdout;
}

function psqlJson(databaseUrl, sql, variables = {}) {
	const args = ["--tuples-only", "--no-align"];
	for (const [name, value] of Object.entries(variables)) {
		args.push("--set", `${name}=${value}`);
	}
	args.push("--command", sql);
	const stdout = runPsql(databaseUrl, args);
	const trimmed = stdout.trim();
	if (!trimmed) {
		return null;
	}
	return JSON.parse(trimmed);
}

function qident(identifier) {
	if (!/^[a-z_][a-z0-9_]*$/u.test(identifier)) {
		throw new Error(`unsafe SQL identifier: ${identifier}`);
	}
	return `"${identifier.replaceAll('"', '""')}"`;
}

function sqlLiteral(value) {
	return `'${String(value).replaceAll("'", "''")}'`;
}

function oldNamePrefilter(columnExpression) {
	return OLD_TOOL_NAMES.map((oldName) => `position(${sqlLiteral(oldName)} in ${columnExpression}) > 0`).join(" or ");
}

function columnExistsSql(table, column) {
	return `
		select jsonb_build_object(
			'exists',
			exists (
				select 1
				from pg_attribute
				where attrelid = to_regclass(${sqlLiteral(table)})
					and attname = ${sqlLiteral(column)}
					and not attisdropped
			)
		)
	`;
}

function discoverCandidate(databaseUrl, candidate, sessionId) {
	const exists = psqlJson(databaseUrl, columnExistsSql(candidate.table, candidate.column));
	if (!exists?.exists) {
		return { ...candidate, exists: false, skippedReason: "table/column not present in current schema" };
	}
	if (sessionId && !candidate.sessionColumn) {
		return { ...candidate, exists: true, skippedReason: "global column skipped when --session-id is used" };
	}
	return { ...candidate, exists: true };
}

function selectRowsSql(candidate, sessionId) {
	const table = qident(candidate.table);
	const column = qident(candidate.column);
	const keysJson = `jsonb_build_object(${candidate.keyColumns
		.flatMap((key) => [sqlLiteral(key), qident(key)])
		.join(", ")})`;
	const whereParts = [oldNamePrefilter(candidate.type === "jsonb" ? `${column}::text` : column)];
	if (sessionId && candidate.sessionColumn) {
		whereParts.unshift(`${qident(candidate.sessionColumn)} = ${sqlLiteral(sessionId)}`);
	}
	const where = `where ${whereParts.map((part) => `(${part})`).join(" and ")}`;
	const valueExpr = candidate.type === "jsonb" ? column : `to_jsonb(${column})`;
	const orderBy = candidate.keyColumns.map((key) => `${qident(key)}::text`).join(", ");
	return `
		select coalesce(jsonb_agg(row_payload order by ordinal), '[]'::jsonb)
		from (
			select ${keysJson} as keys, ${valueExpr} as value, row_number() over (order by ${orderBy}) as ordinal
			from ${table}
			${where}
		) row_payload
	`;
}

function loadRows(databaseUrl, candidate, sessionId) {
	const rows = psqlJson(databaseUrl, selectRowsSql(candidate, sessionId));
	return Array.isArray(rows) ? rows : [];
}

function updateStatement(candidate, updateIndex) {
	const table = qident(candidate.table);
	const column = qident(candidate.column);
	const valuePlaceholder = candidate.type === "jsonb" ? `$${updateIndex}::jsonb` : `$${updateIndex}`;
	const keyPredicates = candidate.keyColumns
		.map((key, keyIndex) => `${qident(key)}::text = $${updateIndex + keyIndex + 1}`)
		.join(" and ");
	return {
		text: `update ${table} set ${column} = ${valuePlaceholder} where ${keyPredicates};`,
		nextIndex: updateIndex + 1 + candidate.keyColumns.length,
	};
}

function psqlDollarQuote(value) {
	const text = String(value);
	let tag = "$pi_relay_migration$";
	let suffix = 0;
	while (text.includes(tag)) {
		suffix += 1;
		tag = `$pi_relay_migration_${suffix}$`;
	}
	return `${tag}${text}${tag}`;
}

function buildApplySql(plan) {
	const parts = [
		"begin;",
		"set local lock_timeout = '5s';",
		"set local statement_timeout = '0';",
		"\\set ON_ERROR_STOP on",
	];
	let preparedIndex = 1;
	for (const columnPlan of plan.columns) {
		if (columnPlan.updates.length === 0) {
			continue;
		}
		const statementName = `migrate_delegation_tool_names_${preparedIndex}`;
		const update = updateStatement(columnPlan.candidate, 1);
		const paramTypes = [
			columnPlan.candidate.type === "jsonb" ? "jsonb" : "text",
			...columnPlan.candidate.keyColumns.map(() => "text"),
		];
		parts.push(`prepare ${statementName} (${paramTypes.join(", ")}) as ${update.text}`);
		for (const row of columnPlan.updates) {
			const value =
				columnPlan.candidate.type === "jsonb" ? JSON.stringify(row.newValue) : String(row.newValue);
			const params = [
				psqlDollarQuote(value),
				...columnPlan.candidate.keyColumns.map((key) => psqlDollarQuote(row.keys[key])),
			];
			parts.push(`execute ${statementName}(${params.join(", ")});`);
		}
		parts.push(`deallocate ${statementName};`);
		preparedIndex += 1;
	}
	parts.push("commit;");
	return `${parts.join("\n")}\n`;
}

function analyzeRows(candidate, rows) {
	const columnCounts = zeroCounts();
	const updates = [];
	for (const row of rows) {
		const migration =
			candidate.type === "jsonb" ? migrateJsonValue(row.value) : migrateTextValue(row.value);
		if (!migration.changed) {
			continue;
		}
		addCounts(columnCounts, migration.counts);
		updates.push({
			keys: row.keys,
			oldValue: row.value,
			newValue: migration.value,
			counts: migration.counts,
		});
	}
	return {
		candidate,
		rowsScanned: rows.length,
		rowsChanged: updates.length,
		occurrences: columnCounts,
		updates,
	};
}

function printCandidateCoverage(discovered) {
	console.log("Candidate columns:");
	for (const candidate of discovered) {
		const name = `${candidate.table}.${candidate.column}`;
		if (!candidate.exists) {
			console.log(`  - ${name}: skipped (${candidate.skippedReason})`);
		} else if (candidate.skippedReason) {
			console.log(`  - ${name}: skipped (${candidate.skippedReason})`);
		} else {
			console.log(`  - ${name}: ${candidate.reason}`);
		}
	}
	console.log("");
}

function printPlan(plan, { apply, sessionId }) {
	const totals = zeroCounts();
	let totalRowsScanned = 0;
	let totalRowsChanged = 0;

	console.log(`Mode: ${apply ? "APPLY" : "DRY-RUN (read-only)"}`);
	if (sessionId) {
		console.log(`Session scope: ${sessionId} (exact session only; child/subagent sessions not included)`);
	} else {
		console.log("Session scope: all sessions (unscoped)");
	}
	console.log("");

	console.log("Planned changes:");
	for (const column of plan.columns) {
		totalRowsScanned += column.rowsScanned;
		totalRowsChanged += column.rowsChanged;
		addCounts(totals, column.occurrences);
		console.log(
			`  - ${column.candidate.table}.${column.candidate.column}: scanned ${column.rowsScanned}, rows changed ${column.rowsChanged}, replacements ${totalCount(column.occurrences)}`,
		);
		for (const oldName of OLD_TOOL_NAMES) {
			if (column.occurrences[oldName]) {
				console.log(`      ${oldName} -> ${TOOL_NAME_MAPPINGS[oldName]}: ${column.occurrences[oldName]}`);
			}
		}
		for (const row of column.updates) {
			const keys = column.candidate.keyColumns.map((key) => `${key}=${JSON.stringify(row.keys[key])}`).join(", ");
			const replacements = OLD_TOOL_NAMES.filter((oldName) => row.counts[oldName])
				.map((oldName) => `${oldName}:${row.counts[oldName]}`)
				.join(", ");
			console.log(`      row ${keys}: ${replacements}`);
		}
	}
	console.log("");
	console.log(`Total rows scanned: ${totalRowsScanned}`);
	console.log(`Total rows changed: ${totalRowsChanged}`);
	console.log(`Total replacements: ${totalCount(totals)}`);
	for (const oldName of OLD_TOOL_NAMES) {
		console.log(`  ${oldName} -> ${TOOL_NAME_MAPPINGS[oldName]}: ${totals[oldName]}`);
	}
	console.log("");
	if (!apply) {
		console.log("Dry-run only: no database rows were modified. Re-run with --apply to mutate.");
	} else {
		console.log("Apply requested: updates will be executed in a single transaction.");
	}
}

async function runMigration(options) {
	if (!options.databaseUrl) {
		throw new Error("DATABASE_URL or --database-url is required");
	}

	const discovered = CANDIDATE_COLUMNS.map((candidate) =>
		discoverCandidate(options.databaseUrl, candidate, options.sessionId),
	);
	printCandidateCoverage(discovered);

	const columns = [];
	for (const candidate of discovered) {
		if (!candidate.exists || candidate.skippedReason) {
			continue;
		}
		const rows = loadRows(options.databaseUrl, candidate, options.sessionId);
		columns.push(analyzeRows(candidate, rows));
	}

	const plan = { columns };
	printPlan(plan, options);

	if (!options.apply) {
		return;
	}

	if (plan.columns.every((column) => column.updates.length === 0)) {
		console.log("No changes to apply.");
		return;
	}

	console.log("IMPORTANT: This script assumes you already stopped the daemon and took a pg_dump backup.");
	console.log("Applying updates...");
	const sql = buildApplySql(plan);
	runPsql(options.databaseUrl, [], sql);
	console.log("Migration committed.");
}

function runSelfTest() {
	const sample = {
		type: "assistant",
		non_exact_stage_status_key: "JSON object keys only migrate when exactly equal to an old tool name",
		items: [
			{
				type: "tool_call",
				tool_name: "stage_start_full",
				text: "Please use stage_start_readonly_fanout, not stage.start_full.",
			},
			{
				type: "provider_replay",
				raw_json: JSON.stringify({
					tools: [{ name: "stage_status" }, { name: "stage.cancel" }],
					nested: JSON.stringify({ tool_name: "stage_cancel" }),
					non_exact_stage_cancel_key: "must remain unchanged",
				}),
			},
		],
		stage_start_full: "key should migrate only when the key is exactly an old tool name",
		summary: "free text can mention stage_start_full and should migrate it",
	};

	const migrated = migrateJsonValue(sample);
	assert.equal(migrated.changed, true);
	assert.equal(migrated.value.items[0].tool_name, "delegate_writing_task");
	assert.equal(
		migrated.value.items[0].text,
		"Please use delegate_readonly_tasks, not stage.start_full.",
		"dotted web RPC name must be preserved",
	);
	assert.equal(migrated.value.delegate_writing_task, sample.stage_start_full);
	assert.equal(migrated.value.stage_start_full, undefined);
	const raw = JSON.parse(migrated.value.items[1].raw_json);
	assert.equal(raw.tools[0].name, "inspect_delegation");
	assert.equal(raw.tools[1].name, "stage.cancel");
	assert.equal(JSON.parse(raw.nested).tool_name, "cancel_delegation");
	assert.equal(raw.non_exact_stage_cancel_key, "must remain unchanged");
	assert.equal(
		migrated.value.non_exact_stage_status_key,
		"JSON object keys only migrate when exactly equal to an old tool name",
	);
	assert.equal(migrated.counts.stage_start_full, 3);
	assert.equal(migrated.counts.stage_start_readonly_fanout, 1);
	assert.equal(migrated.counts.stage_status, 1);
	assert.equal(migrated.counts.stage_cancel, 1);

	const dotted = migrateJsonValue({
		rpc: "stage.start_full stage.start_readonly_fanout stage.status stage.cancel stage.list",
	});
	assert.equal(dotted.changed, false);

	const noOp = analyzeRows(
		{
			table: "transcript_entries",
			column: "item",
			type: "jsonb",
			keyColumns: ["session_id", "id"],
		},
		[{ keys: { session_id: "s", id: "e" }, value: { tool_name: "delegate_writing_task" } }],
	);
	assert.equal(noOp.rowsChanged, 0);

	const changed = analyzeRows(
		{
			table: "actions",
			column: "payload",
			type: "jsonb",
			keyColumns: ["id"],
		},
		[{ keys: { id: "a" }, value: { tool_call: { tool_name: "stage_start_readonly_fanout" } } }],
	);
	assert.equal(changed.rowsChanged, 1);
	assert.equal(changed.updates[0].newValue.tool_call.tool_name, "delegate_readonly_tasks");

	const text = migrateTextValue("Use stage_status, but web RPC stage.status stays.");
	assert.equal(text.value, "Use inspect_delegation, but web RPC stage.status stays.");

	const invalidObjectString = migrateJsonValue('{"tool_name":"stage_status",}');
	assert.equal(invalidObjectString.changed, true);
	assert.equal(invalidObjectString.value, '{"tool_name":"inspect_delegation",}');
	assert.equal(invalidObjectString.counts.stage_status, 1);

	const invalidArrayString = migrateJsonValue('["stage_start_full",]');
	assert.equal(invalidArrayString.changed, true);
	assert.equal(invalidArrayString.value, '["delegate_writing_task",]');
	assert.equal(invalidArrayString.counts.stage_start_full, 1);

	for (const primitive of [null, 0, 42, true, false]) {
		const result = migrateJsonValue(primitive);
		assert.equal(result.changed, false);
		assert.equal(result.value, primitive);
		assert.equal(totalCount(result.counts), 0);
	}

	const nullText = migrateTextValue(null);
	assert.equal(nullText.changed, false);
	assert.equal(nullText.value, null);
	assert.equal(totalCount(nullText.counts), 0);

	console.log("Self-test passed.");
}

async function main() {
	const options = parseArgs(process.argv.slice(2));
	if (options.help) {
		printHelp();
		return;
	}
	if (options.selfTest) {
		runSelfTest();
		return;
	}
	await runMigration(options);
}

main().catch((error) => {
	console.error(error instanceof Error ? error.message : String(error));
	process.exitCode = 1;
});
