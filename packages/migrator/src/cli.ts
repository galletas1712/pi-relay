// M10a migrator CLI.
//   node src/cli.ts migrate [--only id1,id2] [--limit N]
//   node src/cli.ts verify  [--limit N] [--tree-out f.json] [--tree-check f.json]
// Safety: reads ONLY the scratch restore (see config.ts); never live pi-relay state.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { loadConfig } from "./config.ts";
import { SourceDb, type DelegationRow, type SessionRow } from "./pgsource.ts";
import { emitSession, fileStamp, type ChildLink, type SessionResult } from "./sessions.ts";
import { emitControlPlaneSql, emitWorkspacePlan, emitMcpManifests, emitAuditJsonl, emitManifest } from "./emit-control.ts";
import { emitHarnessState, emitMcpToml, emitRolesSkillsScopes } from "./emit-config.ts";
import { sessionUuid, sha256hex } from "./ids.ts";
import { treeCompare, treeSnapshot, verifySession } from "./verify.ts";

function arg(flag: string): string | undefined {
	const i = process.argv.indexOf(flag);
	return i >= 0 ? process.argv[i + 1] : undefined;
}

const DUMP_LABEL = process.env.MIGRATE_DUMP_LABEL ?? "20260809-112741";

async function main(): Promise<void> {
	const cmd = process.argv[2];
	const cfg = loadConfig();
	const db = new SourceDb({ host: cfg.pgHost, port: cfg.pgPort, database: cfg.pgDb, user: cfg.pgUser, password: cfg.pgPassword });
	try {
		if (cmd === "migrate") await migrate(cfg, db);
		else if (cmd === "verify") await verify(cfg, db);
		else throw new Error(`usage: cli.ts <migrate|verify> (got ${cmd})`);
	} finally {
		await db.close();
	}
}

function selectSessions(all: SessionRow[]): SessionRow[] {
	const only = arg("--only");
	if (only) {
		const want = new Set(only.split(","));
		return all.filter((s) => want.has(s.id));
	}
	const limit = arg("--limit");
	if (limit) return all.slice(0, Number(limit));
	return all;
}

function buildChildLinks(sessions: SessionRow[], delegations: DelegationRow[]): Map<string, ChildLink[]> {
	const byDelegation = new Map<string, SessionRow[]>();
	for (const s of sessions) {
		if (!s.delegation_id) continue;
		const cur = byDelegation.get(s.delegation_id) ?? [];
		cur.push(s);
		byDelegation.set(s.delegation_id, cur);
	}
	const out = new Map<string, ChildLink[]>();
	for (const d of delegations) {
		const kids = byDelegation.get(d.id) ?? [];
		const links: ChildLink[] = out.get(d.parent_session_id) ?? [];
		if (kids.length === 0) {
			// launch failure etc: no child session exists; record lifecycle with null session_id
			links.push({ delegation: d, childOldId: "", childNewId: "", spawnIndex: null, promptChars: null });
		} else {
			for (const k of kids) {
				const meta = (k.metadata ?? {}) as Record<string, unknown>;
				const idx = typeof meta.delegation_spawn_index === "number" ? meta.delegation_spawn_index : null;
				const task = typeof meta.task === "string" ? meta.task : null;
				links.push({ delegation: d, childOldId: k.id, childNewId: sessionUuid(k.id), spawnIndex: idx, promptChars: task?.length ?? null });
			}
		}
		out.set(d.parent_session_id, links);
	}
	return out;
}

async function migrate(cfg: ReturnType<typeof loadConfig>, db: SourceDb): Promise<void> {
	const t0 = Date.now();
	mkdirSync(cfg.outRoot, { recursive: true });
	const [allSessions, projects, delegations, manifests, actionCounts, queued, events, tableCounts] = await Promise.all([
		db.sessions(),
		db.projects(),
		db.delegations(),
		db.manifests(),
		db.actionCounts(),
		db.queuedInputs(),
		db.events(),
		db.tableCounts(),
	]);
	const sessions = selectSessions(allSessions);
	const byOld = new Map(allSessions.map((s) => [s.id, s]));
	const childLinks = buildChildLinks(allSessions, delegations);
	console.log(`migrate: ${sessions.length}/${allSessions.length} sessions from ${cfg.pgDb} -> ${cfg.outRoot}`);

	const results = new Map<string, SessionResult>();
	let changed = 0;
	let done = 0;
	for (const s of sessions) {
		const rows = await db.entries(s.id);
		const parent = s.parent_session_id ? byOld.get(s.parent_session_id) : undefined;
		const parentFile = parent ? join(cfg.sessionFilePrefix, `${fileStamp(parent.created_at)}_${sessionUuid(parent.id)}.jsonl`) : null;
		const links = (childLinks.get(s.id) ?? []).filter((l) => l.childOldId === "" || sessions === allSessions || selectIncludes(sessions, l.childOldId));
		const r = emitSession(s, rows, links, parentFile, cfg);
		results.set(s.id, r);
		if (r.changed) changed += 1;
		done += 1;
		if (done % 500 === 0) console.log(`  ${done}/${sessions.length} sessions (${changed} changed)`);
	}

	// control-plane + plans (only complete for full runs)
	const full = sessions.length === allSessions.length;
	const cp = emitControlPlaneSql(cfg, sessions, results, projects);
	const wp = emitWorkspacePlan(cfg, sessions);
	const mm = emitMcpManifests(cfg, manifests, sessions);
	const aD = emitAuditJsonl(cfg, "delegations", delegations);
	const aE = emitAuditJsonl(cfg, "events", events);
	const aQ = emitAuditJsonl(cfg, "queued_inputs", queued);
	const mf = emitManifest(cfg, DUMP_LABEL, sessions, results, projects, tableCounts, actionCounts);
	const rs = emitRolesSkillsScopes(cfg);
	const hs = emitHarnessState(cfg, DUMP_LABEL, sessions.length);
	const mc = emitMcpToml(cfg);

	const report = {
		run_at: new Date().toISOString(),
		duration_ms: Date.now() - t0,
		dump: DUMP_LABEL,
		out_root: cfg.outRoot,
		full_run: full,
		sessions_processed: sessions.length,
		session_files_changed: changed,
		artifacts: {
			control_plane_sql_changed: cp.changed,
			workspace_plan_changed: wp.changed,
			mcp_manifests_changed: mm.changed,
			manifest_changed: mf.changed,
			config_plane_changed: rs.changed + hs.changed + mc.changed,
			audit_lines: { delegations: aD.lines, events: aE.lines, queued_inputs: aQ.lines },
		},
		totals: JSON.parse(readFileSync(mf.path, "utf8")).totals,
		source_table_counts: tableCounts,
		mcp_servers: mc.servers,
	};
	writeFileSync(join(cfg.outRoot, "migration-report.json"), JSON.stringify(report, null, 1) + "\n");
	console.log(`migrate done: ${sessions.length} sessions, ${changed} files changed, ${report.duration_ms}ms`);
	console.log(`totals: ${JSON.stringify(report.totals)}`);
}

function selectIncludes(sessions: SessionRow[], oldId: string): boolean {
	return sessions.some((s) => s.id === oldId);
}

async function verify(cfg: ReturnType<typeof loadConfig>, db: SourceDb): Promise<void> {
	const treeOut = arg("--tree-out");
	const treeCheck = arg("--tree-check");
	if (treeOut || treeCheck) {
		const snap = treeSnapshot(cfg.outRoot);
		if (treeOut) {
			writeFileSync(treeOut, JSON.stringify(snap, null, 1) + "\n");
			console.log(`tree snapshot: ${Object.keys(snap).length} files -> ${treeOut}`);
		}
		if (treeCheck) {
			const prev = JSON.parse(readFileSync(treeCheck, "utf8")) as Record<string, string>;
			const diff = treeCompare(prev, snap);
			const ok = diff.added.length === 0 && diff.removed.length === 0 && diff.changed.length === 0;
			console.log(`V3 tree check: ${ok ? "PASS (no changes)" : "FAIL"} added=${diff.added.length} removed=${diff.removed.length} changed=${diff.changed.length}`);
			if (!ok) {
				console.log(JSON.stringify(diff, null, 1).slice(0, 4000));
				process.exitCode = 1;
			}
		}
		return;
	}

	// V1: per-session active-branch chain hash, source (PG) vs emitted (file)
	const allSessions = await db.sessions();
	const sessions = selectSessions(allSessions);
	let ok = 0;
	const failures: unknown[] = [];
	let done = 0;
	for (const s of sessions) {
		const rows = await db.entries(s.id);
		const r = verifySession(cfg, s, rows);
		if (r.ok) ok += 1;
		else failures.push(r);
		done += 1;
		if (done % 500 === 0) console.log(`  verify ${done}/${sessions.length} ok=${ok}`);
	}
	console.log(`V1 chain-hash: ${ok}/${sessions.length} sessions match; failures=${failures.length}`);
	if (failures.length > 0) {
		console.log(JSON.stringify(failures.slice(0, 10), null, 1));
		process.exitCode = 1;
	}
}

main().catch((e) => {
	console.error(e);
	process.exit(1);
});
