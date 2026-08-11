// Scratch-PG access + source row types (pi-relay dump restored to pi_relay_migtest).
// READ-ONLY: every function here is a SELECT. The migrator never writes to PG.
import pg from "pg";

export interface SessionRow {
	id: string;
	project_id: string | null;
	workspaces: unknown; // jsonb: pi-relay workspace decls (snake_case)
	created_at: Date;
	updated_at: Date;
	active_leaf_id: string | null;
	provider_config: { kind?: string; model?: string; reasoning_effort?: string } | null;
	metadata: Record<string, unknown> | null;
	system_prompt: string | null;
	parent_session_id: string | null;
	subagent_type: string | null;
	delegation_id: string | null;
	mcp_manifest_fingerprint: string | null;
	workspace_id: string | null;
}

export interface EntryRow {
	id: string;
	parent_id: string | null;
	timestamp_ms: string; // bigint comes back as string
	sequence: string;
	item: Record<string, unknown>;
	has_replay: boolean;
}

export interface DelegationRow {
	id: string;
	parent_session_id: string;
	workflow: string | null;
	label: string | null;
	kind: string;
	status: string;
	attempt_id: string | null;
	created_at: Date;
	updated_at: Date;
	expected_subagents: unknown;
	launch_key: string | null;
	launch_shape: unknown;
	teardown_target: string | null;
	launch_error: string | null;
}

export interface ProjectRow {
	id: string;
	created_at: Date;
	updated_at: Date;
	name: string;
	workspaces: unknown;
	metadata: Record<string, unknown> | null;
	runtime_id: string | null;
}

export interface ManifestRow {
	fingerprint: string;
	manifest: unknown;
	created_at: Date;
}

export class SourceDb {
	private pool: pg.Pool;
	constructor(cfg: { host: string; port: number; database: string; user: string; password: string }) {
		this.pool = new pg.Pool({ ...cfg, max: 4 });
	}
	async close(): Promise<void> {
		await this.pool.end();
	}
	async sessions(): Promise<SessionRow[]> {
		const r = await this.pool.query(
			`select id, project_id, workspaces, created_at, updated_at, active_leaf_id, provider_config,
			        metadata, system_prompt, parent_session_id, subagent_type, delegation_id,
			        mcp_manifest_fingerprint, workspace_id
			   from sessions order by created_at, id`,
		);
		return r.rows;
	}
	async entries(sessionId: string): Promise<EntryRow[]> {
		const r = await this.pool.query(
			`select id, parent_id, timestamp_ms, sequence, item,
			        (provider_replay is not null and provider_replay::text not in ('null','[]')) as has_replay
			   from transcript_entries where session_id = $1 order by sequence`,
			[sessionId],
		);
		return r.rows;
	}
	async delegations(): Promise<DelegationRow[]> {
		const r = await this.pool.query(
			`select id, parent_session_id, workflow, label, kind, status, attempt_id, created_at, updated_at,
			        expected_subagents, launch_key, launch_shape, teardown_target, launch_error
			   from delegations order by created_at, id`,
		);
		return r.rows;
	}
	async projects(): Promise<ProjectRow[]> {
		const r = await this.pool.query(`select id, created_at, updated_at, name, workspaces, metadata, runtime_id from projects order by name`);
		return r.rows;
	}
	async manifests(): Promise<ManifestRow[]> {
		const r = await this.pool.query(`select fingerprint, manifest, created_at from mcp_session_manifests order by fingerprint`);
		return r.rows;
	}
	/** per-session action counts by kind (audit only; action bodies stay in the dump archive) */
	async actionCounts(): Promise<Map<string, Record<string, number>>> {
		const r = await this.pool.query(`select session_id, kind, count(*)::int as n from actions group by 1,2`);
		const out = new Map<string, Record<string, number>>();
		for (const row of r.rows) {
			const cur = out.get(row.session_id) ?? {};
			cur[row.kind] = row.n;
			out.set(row.session_id, cur);
		}
		return out;
	}
	async queuedInputs(): Promise<unknown[]> {
		const r = await this.pool.query(`select * from queued_inputs order by session_id, id`);
		return r.rows;
	}
	async events(): Promise<unknown[]> {
		const r = await this.pool.query(`select * from events order by id`);
		return r.rows;
	}
	async tableCounts(): Promise<Record<string, number>> {
		const tables = ["projects", "runtimes", "sessions", "transcript_entries", "queued_inputs", "actions", "events", "delegations", "mcp_session_manifests", "daemon_config"];
		const out: Record<string, number> = {};
		for (const t of tables) {
			const r = await this.pool.query(`select count(*)::int as n from ${t}`);
			out[t] = r.rows[0].n;
		}
		return out;
	}
}
