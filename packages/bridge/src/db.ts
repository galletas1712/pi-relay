// Postgres control plane. Control-plane ONLY: sessions registry, command
// journal (durable queue), event spool (resumable stream), idempotency keys.
// Transcripts live as pi session JSONL files on disk — never in PG.
import pg from "pg";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "./config.ts";

const { Pool } = pg;
export const pool = new Pool({
	connectionString: config.pgUrl,
	max: 8,
	idleTimeoutMillis: 30_000,
});

export async function migrate(): Promise<number[]> {
	const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "migrations");
	const files = readdirSync(dir).filter((f) => f.endsWith(".sql")).sort();
	const client = await pool.connect();
	try {
		await client.query(`CREATE TABLE IF NOT EXISTS schema_migrations (
			version INTEGER PRIMARY KEY,
			applied_at TIMESTAMPTZ NOT NULL DEFAULT now())`);
		const { rows } = await client.query("SELECT version FROM schema_migrations");
		const applied = new Set(rows.map((r: { version: number }) => r.version));
		const done: number[] = [];
		for (const f of files) {
			const version = Number(f.split("_")[0]);
			if (applied.has(version)) continue;
			await client.query(readFileSync(path.join(dir, f), "utf8"));
			await client.query("INSERT INTO schema_migrations (version) VALUES ($1) ON CONFLICT DO NOTHING", [version]);
			done.push(version);
		}
		return done;
	} finally {
		client.release();
	}
}

// ---- sessions ---------------------------------------------------------------

export interface SessionRow {
	id: string;
	cwd: string;
	session_file: string | null;
	name: string | null;
	state: string;
	host_pid: number | null;
	host_generation: number;
	last_event_seq: string; // bigint comes back as string
	project_id: string | null;
	parent_session_id: string | null; // M11b: fork lineage (null for roots)
	workspaces: unknown; // SessionWorkspace[] (jsonb)
	mcp_selection: unknown; // McpSessionSelection | null (jsonb)
	created_at: Date;
	updated_at: Date;
}

export async function insertSession(row: {
	id: string;
	cwd: string;
	sessionFile: string | null;
	name?: string | null;
	hostPid: number | null;
	hostGeneration?: number;
	projectId?: string | null;
	parentSessionId?: string | null;
	workspaces?: unknown;
	mcpSelection?: unknown;
}): Promise<void> {
	await pool.query(
		`INSERT INTO sessions (id, cwd, session_file, name, host_pid, state, host_generation, project_id, parent_session_id, workspaces, mcp_selection)
		 VALUES ($1, $2, $3, $4, $5, 'starting', $6, $7, $8, $9, $10)`,
		[
			row.id,
			row.cwd,
			row.sessionFile,
			row.name ?? null,
			row.hostPid,
			row.hostGeneration ?? 0,
			row.projectId ?? null,
			row.parentSessionId ?? null,
			JSON.stringify(row.workspaces ?? []),
			row.mcpSelection === undefined ? null : JSON.stringify(row.mcpSelection),
		],
	);
}

// ---- projects (M8) --------------------------------------------------------------

export interface ProjectRow {
	id: string;
	name: string;
	workspaces: unknown; // WorkspaceDecl[] (jsonb)
	created_at: Date;
	updated_at: Date;
}

export async function insertProject(row: { id: string; name: string; workspaces: unknown }): Promise<void> {
	await pool.query("INSERT INTO projects (id, name, workspaces) VALUES ($1, $2, $3)", [
		row.id,
		row.name,
		JSON.stringify(row.workspaces ?? []),
	]);
}

export async function getProject(id: string): Promise<ProjectRow | null> {
	const { rows } = await pool.query("SELECT * FROM projects WHERE id = $1", [id]);
	return rows[0] ?? null;
}

export async function listProjects(): Promise<ProjectRow[]> {
	const { rows } = await pool.query("SELECT * FROM projects ORDER BY created_at");
	return rows;
}

export async function updateProject(
	id: string,
	fields: Partial<{ name: string; workspaces: unknown }>,
): Promise<void> {
	const sets: string[] = ["updated_at = now()"];
	const vals: unknown[] = [];
	if (fields.name !== undefined) {
		vals.push(fields.name);
		sets.push(`name = $${vals.length}`);
	}
	if (fields.workspaces !== undefined) {
		vals.push(JSON.stringify(fields.workspaces));
		sets.push(`workspaces = $${vals.length}`);
	}
	vals.push(id);
	await pool.query(`UPDATE projects SET ${sets.join(", ")} WHERE id = $${vals.length}`, vals);
}

export async function deleteProject(id: string): Promise<void> {
	// ON DELETE SET NULL clears sessions.project_id
	await pool.query("DELETE FROM projects WHERE id = $1", [id]);
}

export async function deleteSessionRow(id: string): Promise<void> {
	await pool.query("DELETE FROM sessions WHERE id = $1", [id]);
}

export async function updateSession(
	id: string,
	fields: Partial<{ state: string; hostPid: number | null; sessionFile: string; name: string; hostGeneration: number; mcpSelection: unknown; parentSessionId: string | null }>,
): Promise<void> {
	const sets: string[] = ["updated_at = now()"];
	const vals: unknown[] = [];
	const push = (col: string, v: unknown) => {
		vals.push(v);
		sets.push(`${col} = $${vals.length}`);
	};
	if (fields.state !== undefined) push("state", fields.state);
	if (fields.hostPid !== undefined) push("host_pid", fields.hostPid);
	if (fields.sessionFile !== undefined) push("session_file", fields.sessionFile);
	if (fields.parentSessionId !== undefined) push("parent_session_id", fields.parentSessionId);
	if (fields.name !== undefined) push("name", fields.name);
	if (fields.hostGeneration !== undefined) push("host_generation", fields.hostGeneration);
	if (fields.mcpSelection !== undefined) push("mcp_selection", fields.mcpSelection === null ? null : JSON.stringify(fields.mcpSelection));
	vals.push(id);
	await pool.query(`UPDATE sessions SET ${sets.join(", ")} WHERE id = $${vals.length}`, vals);
}

export async function getSession(id: string): Promise<SessionRow | null> {
	const { rows } = await pool.query("SELECT * FROM sessions WHERE id = $1", [id]);
	return rows[0] ?? null;
}

export async function listSessions(): Promise<SessionRow[]> {
	const { rows } = await pool.query("SELECT * FROM sessions ORDER BY created_at");
	return rows;
}

// ---- event spool (resumable stream; per-session monotonic seq) --------------
// The in-memory supervisor assigns seqs; these helpers persist + read them.

export async function spoolInsert(sessionId: string, seq: number, event: string, payload: unknown): Promise<void> {
	await pool.query(
		`WITH ins AS (
		   INSERT INTO event_spool (session_id, seq, event, payload) VALUES ($1, $2, $3, $4)
		 )
		 UPDATE sessions SET last_event_seq = $2, updated_at = now() WHERE id = $1`,
		[sessionId, seq, event, JSON.stringify(payload)],
	);
	// ring buffer trim
	await pool.query("DELETE FROM event_spool WHERE session_id = $1 AND seq <= $2::bigint - $3::int", [
		sessionId,
		seq,
		config.spoolCap,
	]);
}

export interface SpoolRow {
	session_id: string;
	seq: string;
	event: string;
	payload: unknown;
	created_at: Date;
}

export async function spoolRead(sessionId: string, afterSeq: number, limit = 10_000): Promise<SpoolRow[]> {
	const { rows } = await pool.query(
		"SELECT * FROM event_spool WHERE session_id = $1 AND seq > $2 ORDER BY seq LIMIT $3",
		[sessionId, afterSeq, limit],
	);
	return rows;
}

export async function spoolMinSeq(sessionId: string): Promise<number> {
	const { rows } = await pool.query("SELECT min(seq) AS m FROM event_spool WHERE session_id = $1", [sessionId]);
	return rows[0]?.m === null || rows[0]?.m === undefined ? 0 : Number(rows[0].m);
}

// ---- command journal (durable user-facing queue) ----------------------------

export interface JournalRow {
	seq: string;
	session_id: string;
	host_generation: number;
	kind: string;
	payload: { text?: string; [k: string]: unknown };
	status: string;
	error: string | null;
	idempotency_key: string | null;
	created_at: Date;
	acked_at: Date | null;
}

export async function journalInsert(entry: {
	sessionId: string;
	hostGeneration: number;
	kind: string;
	payload: unknown;
	idempotencyKey?: string | null;
}): Promise<number> {
	const { rows } = await pool.query(
		`INSERT INTO command_journal (session_id, host_generation, kind, payload, idempotency_key)
		 VALUES ($1, $2, $3, $4, $5) RETURNING seq`,
		[entry.sessionId, entry.hostGeneration, entry.kind, JSON.stringify(entry.payload), entry.idempotencyKey ?? null],
	);
	return Number(rows[0].seq);
}

export async function journalSetStatus(seq: number, status: string, error?: string): Promise<void> {
	await pool.query(
		`UPDATE command_journal SET status = $2, error = $3,
		 acked_at = CASE WHEN $2 = 'acked' THEN now() ELSE acked_at END
		 WHERE seq = $1`,
		[seq, status, error ?? null],
	);
}

export async function journalPending(sessionId: string): Promise<JournalRow[]> {
	const { rows } = await pool.query(
		"SELECT * FROM command_journal WHERE session_id = $1 AND status = 'pending' ORDER BY seq",
		[sessionId],
	);
	return rows;
}

/** On host crash: entries acked but possibly never consumed by pi (its
 * steer/follow-up queues are in-memory only) are marked maybe_lost. */
export async function journalMarkMaybeLost(sessionId: string): Promise<number> {
	const { rowCount } = await pool.query(
		`UPDATE command_journal SET status = 'maybe_lost'
		 WHERE session_id = $1 AND status = 'acked' AND kind IN ('steer', 'follow_up')`,
		[sessionId],
	);
	return rowCount ?? 0;
}

// ---- idempotency keys --------------------------------------------------------

export function paramsHash(params: unknown): string {
	return createHash("sha256").update(JSON.stringify(params ?? null)).digest("hex");
}

export interface IdemRow {
	key: string;
	method: string;
	session_id: string | null;
	params_hash: string;
	response: unknown;
	created_at: Date;
}

export async function idemGet(key: string): Promise<IdemRow | null> {
	const { rows } = await pool.query("SELECT * FROM idempotency_keys WHERE key = $1", [key]);
	return rows[0] ?? null;
}

export async function idemPut(row: {
	key: string;
	method: string;
	sessionId?: string | null;
	paramsHash: string;
	response: unknown;
}): Promise<void> {
	await pool.query(
		`INSERT INTO idempotency_keys (key, method, session_id, params_hash, response)
		 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (key) DO NOTHING`,
		[row.key, row.method, row.sessionId ?? null, row.paramsHash, JSON.stringify(row.response)],
	);
}

export async function closeDb(): Promise<void> {
	await pool.end();
}
