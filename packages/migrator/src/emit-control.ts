// Control-plane + plan emitters: control-plane.sql (bridge projects+sessions),
// workspace-plan.json (PLAN ONLY — no btrfs, no live state), mcp manifest copies,
// audit exports, and the global idmap manifest.
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import type { DelegationRow, ManifestRow, ProjectRow, SessionRow } from "./pgsource.ts";
import { projectUuid, sessionUuid } from "./ids.ts";
import { fileStamp, writeIfChanged, type SessionResult } from "./sessions.ts";
import type { MigratorConfig } from "./config.ts";

function sqlLit(v: string | null): string {
	if (v === null) return "NULL";
	return `'${v.replaceAll("'", "''")}'`;
}
function sqlJson(v: unknown): string {
	return `${sqlLit(JSON.stringify(v))}::jsonb`;
}
function sqlTs(d: Date | string): string {
	return sqlLit(d instanceof Date ? d.toISOString() : new Date(d).toISOString());
}

interface WsDecl {
	kind?: string;
	workspace_dir?: string;
	remote_url?: string;
	remote_branch?: string;
	local_branch?: string;
	base_sha?: string;
	source_path?: string;
}

/** pi-relay workspace decl (snake_case) -> bridge WorkspaceDecl (camelCase). */
export function toBridgeDecl(w: WsDecl): Record<string, unknown> {
	if (w.kind === "git") {
		const out: Record<string, unknown> = { kind: "git", workspaceDir: w.workspace_dir, remoteUrl: w.remote_url, remoteBranch: w.remote_branch };
		if (w.local_branch) out.branchOverride = w.local_branch;
		return out;
	}
	return { kind: "local", workspaceDir: w.workspace_dir, sourcePath: w.source_path };
}

export function emitControlPlaneSql(
	cfg: MigratorConfig,
	sessions: SessionRow[],
	results: Map<string, SessionResult>,
	projects: ProjectRow[],
): { path: string; changed: boolean } {
	const lines: string[] = [];
	lines.push("-- M10a migrator control-plane import (pi-relay dump -> bridge PG).");
	lines.push("-- Idempotent: every row INSERT ... ON CONFLICT DO NOTHING. Sessions are emitted with");
	lines.push(`-- state='${cfg.emitState}' so a bridge restart does NOT mass-respawn them (see M10-MIGRATOR.md).`);
	lines.push("-- session_file paths are absolute to MIGRATE_OUT at emission time; re-emit to relocate.");
	lines.push("BEGIN;");
	for (const p of projects) {
		const newId = projectUuid(p.id);
		const decls = (Array.isArray(p.workspaces) ? (p.workspaces as WsDecl[]) : []).map(toBridgeDecl);
		lines.push(
			`INSERT INTO projects (id, name, workspaces, created_at, updated_at) VALUES (` +
				`${sqlLit(newId)}, ${sqlLit(p.name)}, ${sqlJson(decls)}, ${sqlTs(p.created_at)}, ${sqlTs(p.updated_at)}` +
				`) ON CONFLICT (id) DO NOTHING;`,
		);
	}
	for (const s of sessions) {
		const r = results.get(s.id);
		if (!r) throw new Error(`missing session result for ${s.id}`);
		const meta = (s.metadata ?? {}) as Record<string, unknown>;
		const title = typeof meta.title === "string" ? meta.title : null;
		const projectId = s.project_id ? projectUuid(s.project_id) : null;
		lines.push(
			`INSERT INTO sessions (id, cwd, session_file, name, state, host_generation, last_event_seq, created_at, updated_at, project_id, workspaces, mcp_selection) VALUES (` +
				`${sqlLit(r.newId)}, ${sqlLit(join(cfg.outRoot, "cwd", r.newId))}, ${sqlLit(join(cfg.sessionFilePrefix, `${fileStamp(s.created_at)}_${r.newId}.jsonl`))}, ` +
				`${sqlLit(title)}, ${sqlLit(cfg.emitState)}, 0, 0, ${sqlTs(s.created_at)}, ${sqlTs(s.updated_at)}, ${sqlLit(projectId)}, '[]'::jsonb, NULL` +
				`) ON CONFLICT (id) DO NOTHING;`,
		);
	}
	lines.push("COMMIT;");
	const path = join(cfg.outRoot, "control-plane.sql");
	const changed = writeIfChanged(path, lines.join("\n") + "\n");
	return { path, changed };
}

export function emitWorkspacePlan(cfg: MigratorConfig, sessions: SessionRow[]): { path: string; changed: boolean } {
	const plan = {
		version: 1,
		kind: "pi-relay workspace migration plan (M10a) — PLAN ONLY, no btrfs/subvolume operations performed",
		source_state_root: "~/.local/state/pi-relay (NEVER touched by the migrator; M11 transfers content)",
		new_stack_target: "bridge workspaces.ts materializeForSession (managed git/local workspaces, session subvolumes)",
		sessions: sessions.map((s) => {
			const decls = (Array.isArray(s.workspaces) ? (s.workspaces as WsDecl[]) : []) as WsDecl[];
			return {
				old_session_id: s.id,
				new_session_id: sessionUuid(s.id),
				workspace_id: s.workspace_id,
				source_materialized_path: `~/.local/state/pi-relay/sessions/${s.id}/cwd`,
				decls: decls.map((w) => ({ ...toBridgeDecl(w), baseSha: w.base_sha ?? null, piRelayLocalBranch: w.local_branch ?? null })),
			};
		}),
	};
	const path = join(cfg.outRoot, "workspace-plan.json");
	const changed = writeIfChanged(path, JSON.stringify(plan, null, 1) + "\n");
	return { path, changed };
}

export function emitMcpManifests(cfg: MigratorConfig, manifests: ManifestRow[], sessions: SessionRow[]): { changed: boolean; files: number } {
	const dir = join(cfg.outRoot, "mcp-manifests");
	mkdirSync(dir, { recursive: true });
	let changed = false;
	let files = 0;
	for (const m of manifests) {
		files += 1;
		if (writeIfChanged(join(dir, `${m.fingerprint}.json`), JSON.stringify(m.manifest, null, 1) + "\n")) changed = true;
	}
	// per-session selection sidecar (which servers each session had selected)
	const perSession = sessions
		.filter((s) => s.mcp_manifest_fingerprint)
		.map((s) => ({ old_session_id: s.id, new_session_id: sessionUuid(s.id), manifest_fingerprint: s.mcp_manifest_fingerprint }));
	if (writeIfChanged(join(dir, "by-session.json"), JSON.stringify(perSession, null, 1) + "\n")) changed = true;
	return { changed, files };
}

export function emitAuditJsonl(cfg: MigratorConfig, name: string, rows: unknown[]): { changed: boolean; lines: number } {
	const body = rows.map((r) => JSON.stringify(r)).join("\n") + (rows.length > 0 ? "\n" : "");
	const changed = writeIfChanged(join(cfg.outRoot, "audit", `${name}.jsonl`), body);
	return { changed, lines: rows.length };
}

export function emitManifest(
	cfg: MigratorConfig,
	dumpLabel: string,
	sessions: SessionRow[],
	results: Map<string, SessionResult>,
	projects: ProjectRow[],
	tableCounts: Record<string, number>,
	actionCounts: Map<string, Record<string, number>>,
): { path: string; changed: boolean } {
	const depthOf = (s: SessionRow): number => {
		let d = 0;
		let cur = s;
		const byOld = new Map(sessions.map((x) => [x.id, x]));
		while (cur.parent_session_id) {
			const p = byOld.get(cur.parent_session_id);
			if (!p) break;
			d += 1;
			cur = p;
			if (d > 32) break;
		}
		return d;
	};
	const sorted = [...sessions].sort((a, b) => depthOf(a) - depthOf(b) || String(a.created_at).localeCompare(String(b.created_at)) || a.id.localeCompare(b.id));
	const manifest = {
		version: 1,
		dump: dumpLabel,
		source_table_counts: tableCounts,
		totals: {
			sessions: sessions.length,
			files_emitted: results.size,
			source_entries: [...results.values()].reduce((n, r) => n + r.stats.sourceEntries, 0),
			emitted_entries: [...results.values()].reduce((n, r) => n + r.stats.emittedEntries, 0),
			skipped_by_type: mergeCounts([...results.values()].map((r) => r.stats.skippedByType)),
			emitted_by_type: mergeCounts([...results.values()].map((r) => r.stats.byType)),
			replay_dropped: [...results.values()].reduce((n, r) => n + r.stats.replayDropped, 0),
			args_parse_failures: [...results.values()].reduce((n, r) => n + r.stats.argsParseFailures, 0),
			compactions: [...results.values()].reduce((n, r) => n + r.stats.compactions, 0),
			cross_session_compactions: [...results.values()].reduce((n, r) => n + r.stats.crossSessionCompactions, 0),
		},
		projects: projects.map((p) => ({ old: p.id, new: projectUuid(p.id), name: p.name })),
		sessions: sorted.map((s) => {
			const r = results.get(s.id)!;
			return {
				old: s.id,
				new: r.newId,
				file: r.relFile,
				sha256: r.fileSha256,
				title: ((s.metadata ?? {}) as Record<string, unknown>).title ?? null,
				depth: depthOf(s),
				parent_old: s.parent_session_id,
				parent_new: s.parent_session_id ? sessionUuid(s.parent_session_id) : null,
				delegation_id: s.delegation_id,
				subagent_type: s.subagent_type,
				action_counts: actionCounts.get(s.id) ?? {},
			};
		}),
	};
	const path = join(cfg.outRoot, "idmap", "manifest.json");
	const changed = writeIfChanged(path, JSON.stringify(manifest, null, 1) + "\n");
	return { path, changed };
}

function mergeCounts(list: Array<Record<string, number>>): Record<string, number> {
	const out: Record<string, number> = {};
	for (const c of list) for (const [k, v] of Object.entries(c)) out[k] = (out[k] ?? 0) + v;
	return out;
}
