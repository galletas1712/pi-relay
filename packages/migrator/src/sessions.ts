// Per-session migration: source rows -> pi v3 JSONL file + idmap sidecar.
// Deterministic: file bytes depend only on source rows (V3 idempotency).
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { DelegationRow, EntryRow, SessionRow } from "./pgsource.ts";
import { convertSessionEntries, type ConvStats, type V3Entry } from "./convert.ts";
import { sessionUuid, sha256hex } from "./ids.ts";
import type { MigratorConfig } from "./config.ts";

export interface SessionResult {
	oldId: string;
	newId: string;
	file: string; // absolute path of emitted session file
	relFile: string; // path relative to outRoot
	changed: boolean; // file bytes changed on disk (V3: second run -> false)
	stats: ConvStats;
	fileSha256: string;
	activeLeafNewId: string | null;
}

function iso(d: Date | string): string {
	return d instanceof Date ? d.toISOString() : new Date(d).toISOString();
}

export function fileStamp(d: Date | string): string {
	return iso(d).replaceAll(":", "-").replaceAll(".", "-");
}

export interface ChildLink {
	delegation: DelegationRow;
	childOldId: string;
	childNewId: string;
	spawnIndex: number | null;
	promptChars: number | null;
}

function lifecyclePhase(status: string): { phase: string; status: string } {
	switch (status) {
		case "done":
			return { phase: "completed", status: "completed" };
		case "done_with_failures":
			return { phase: "completed", status: "done_with_failures" };
		case "failed":
			return { phase: "failed", status: "failed" };
		case "cancelled":
			return { phase: "failed", status: "cancelled" };
		default:
			return { phase: "admitted", status: status }; // 'running' at snapshot: admitted only (stale; M11 note)
	}
}

/** Build the full v3 line array for one session. Pure (no IO) for testability. */
export function buildSessionLines(
	session: SessionRow,
	rows: EntryRow[],
	childLinks: ChildLink[],
	parentNewFile: string | null, // emitted file of parent_session_id (fork lineage), absolute
	cfg: MigratorConfig,
): { lines: string[]; result: Omit<SessionResult, "changed" | "file" | "relFile" | "fileSha256">; idMapJson: Record<string, unknown> } {
	const newId = sessionUuid(session.id);
	const conv = convertSessionEntries(session.id, rows, session.active_leaf_id, session.provider_config);
	const created = iso(session.created_at);
	const updated = iso(session.updated_at);
	const cwd = join(cfg.outRoot, "cwd", newId);
	const entries: V3Entry[] = [];

	// header (v3). parentSession = fork lineage (pi-relay deep-copy fork w/o delegation).
	const header: V3Entry = { type: "session", version: 3, id: newId, timestamp: created, cwd };
	if (session.parent_session_id && !session.delegation_id && parentNewFile) header.parentSession = parentNewFile;
	entries.push(header);

	// synthetic head (off active path; provenance + context settings)
	const synth = (label: string): string => sha256hex(`${session.id}:synthetic:${label}`).slice(0, 8);
	const pc = session.provider_config ?? {};
	entries.push({
		type: "model_change",
		id: synth("model_change"),
		parentId: null,
		timestamp: created,
		provider: pc.kind ?? "openai",
		modelId: pc.model ?? "unknown",
	});
	const effort = String(pc.reasoning_effort ?? "").toLowerCase();
	const thinking = effort === "none" || effort === "off" ? "off" : ["minimal", "low", "medium", "high", "xhigh", "max"].includes(effort) ? effort : null;
	if (thinking) {
		entries.push({ type: "thinking_level_change", id: synth("thinking"), parentId: synth("model_change"), timestamp: created, thinkingLevel: thinking });
	}
	// pi v3 native layout: model_change -> thinking_level_change -> first message, all on the
	// ACTIVE path, so a resumed session restores its historical model/thinking (with graceful
	// fallback to the settings default when the provider is not configured in the new env).
	const headTip = thinking ? synth("thinking") : synth("model_change");
	const meta = session.metadata ?? {};
	entries.push({
		type: "custom",
		customType: "pi_relay_session_info",
		id: synth("session_info"),
		parentId: synth("model_change"),
		timestamp: created,
		data: {
			source: "pi-relay",
			source_session_id: session.id,
			provider_config: session.provider_config ?? null,
			subagent_type: session.subagent_type,
			delegation_id: session.delegation_id,
			parent_session_id: session.parent_session_id,
			mcp_manifest_fingerprint: session.mcp_manifest_fingerprint,
			prompt_profile: meta.prompt_profile ?? null,
			role_name: meta.role_name ?? null,
			hidden: meta.hidden ?? null,
			system_prompt_sha256: session.system_prompt ? sha256hex(session.system_prompt) : null,
			system_prompt_chars: session.system_prompt?.length ?? 0,
		},
	});

	// converted transcript; re-anchor the first (root) entry under the head chain
	const convEntries = conv.entries;
	if (convEntries.length > 0 && convEntries[0].parentId === null) {
		convEntries[0] = { ...convEntries[0], parentId: headTip };
	}
	entries.push(...convEntries);

	// delegation lifecycle (parent side), readable by bridge supervisor.subagentTree
	for (const link of childLinks) {
		const d = link.delegation;
		const rlmChildId = link.childNewId.replaceAll("-", "").slice(0, 8);
		const term = lifecyclePhase(d.status);
		entries.push({
			type: "custom",
			customType: "rlm_child_lifecycle",
			id: synth(`lifecycle:${d.id}:${link.childOldId}:admitted`),
			parentId: conv.activeLeafNewId,
			timestamp: iso(d.created_at),
			data: {
				phase: "admitted",
				rlm_child_id: rlmChildId,
				session_name: d.label ?? d.workflow ?? null,
				session_id: null,
				status: "running",
				model: null,
				created_at: iso(d.created_at),
				prompt_chars: link.promptChars,
			},
		});
		if (term.phase !== "admitted") {
			entries.push({
				type: "custom",
				customType: "rlm_child_lifecycle",
				id: synth(`lifecycle:${d.id}:${link.childOldId}:${term.phase}`),
				parentId: conv.activeLeafNewId,
				timestamp: iso(d.updated_at),
				data: {
					phase: term.phase,
					rlm_child_id: rlmChildId,
					session_name: d.label ?? d.workflow ?? null,
					session_id: link.childNewId,
					status: term.status,
					model: null,
					created_at: iso(d.created_at),
					result_preview: null,
					error: d.launch_error ?? (term.phase === "failed" ? `pi-relay delegation ${d.status}` : null),
				},
			});
		}
	}

	// migration marker: ALWAYS the last line -> pins the v3 leaf at the pi-relay active leaf.
	entries.push({
		type: "custom",
		customType: "pi_relay_migration",
		id: synth("migration_marker"),
		parentId: conv.activeLeafNewId ?? headTip,
		timestamp: updated,
		data: {
			source: "pi-relay",
			source_session_id: session.id,
			source_created_at: created,
			source_updated_at: updated,
			new_session_id: newId,
			subagent_type: session.subagent_type,
			delegation_id: session.delegation_id,
			parent_session_id_old: session.parent_session_id,
			parent_session_id_new: session.parent_session_id ? sessionUuid(session.parent_session_id) : null,
			delegation_spawn_index: meta.delegation_spawn_index ?? null,
			mcp_manifest_fingerprint: session.mcp_manifest_fingerprint,
			source_entries: conv.stats.sourceEntries,
			emitted_entries: conv.stats.emittedEntries,
		},
	});

	const lines = entries.map((e) => JSON.stringify(e));
	const idMapJson: Record<string, unknown> = {
		old_session_id: session.id,
		new_session_id: newId,
		title: (meta.title as string) ?? null,
		project_id_old: session.project_id,
		subagent_type: session.subagent_type,
		delegation_id: session.delegation_id,
		parent_session_id_old: session.parent_session_id,
		parent_session_id_new: session.parent_session_id ? sessionUuid(session.parent_session_id) : null,
		entry_id_map: conv.idMap,
		active_leaf_old: session.active_leaf_id,
		active_leaf_new: conv.activeLeafNewId,
		counts: conv.stats,
	};
	return {
		lines,
		result: { oldId: session.id, newId, stats: conv.stats, activeLeafNewId: conv.activeLeafNewId },
		idMapJson,
	};
}

/** write-if-different; returns true when bytes changed. */
export function writeIfChanged(path: string, content: string): boolean {
	if (existsSync(path) && readFileSync(path, "utf8") === content) return false;
	mkdirSync(join(path, ".."), { recursive: true });
	writeFileSync(path, content);
	return true;
}

export function emitSession(
	session: SessionRow,
	rows: EntryRow[],
	childLinks: ChildLink[],
	parentNewFile: string | null,
	cfg: MigratorConfig,
): SessionResult {
	const { lines, result, idMapJson } = buildSessionLines(session, rows, childLinks, parentNewFile, cfg);
	const sessionsDir = join(cfg.outRoot, "sessions");
	mkdirSync(sessionsDir, { recursive: true });
	const file = join(sessionsDir, `${fileStamp(session.created_at)}_${result.newId}.jsonl`);
	const body = lines.join("\n") + "\n";
	const changed = writeIfChanged(file, body);
	const fileSha256 = createHash("sha256").update(body, "utf8").digest("hex");
	// cwd dir for bootability (real dir; workspace CONTENT is an M11 plan-file matter)
	mkdirSync(join(cfg.outRoot, "cwd", result.newId), { recursive: true });
	// idmap sidecar
	const sidecar = { ...idMapJson, file: file, file_sha256: fileSha256 };
	const sidecarPath = join(cfg.outRoot, "idmap", `${result.newId}.json`);
	writeIfChanged(sidecarPath, JSON.stringify(sidecar, null, 1) + "\n");
	return { ...result, file, relFile: join("sessions", `${fileStamp(session.created_at)}_${result.newId}.jsonl`), changed, fileSha256 };
}
