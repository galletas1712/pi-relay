// Deterministic id helpers (M10a). All ids derive from SOURCE ids so a second
// run over the same dump produces byte-identical output (V3 idempotency).
import { createHash } from "node:crypto";

const MIGRATOR_NS = "6f1e6f2c-9c3a-4f7e-9c1a-b16e00000a10"; // fixed random namespace for all migrator ids

/** RFC 4122 §5.4 uuid v5 (SHA-1, name-based). Deterministic across runs. */
export function uuidV5(namespace: string, name: string): string {
	const ns = namespace.replaceAll("-", "");
	const nsBytes = Buffer.from(ns, "hex");
	const h = createHash("sha1");
	h.update(nsBytes);
	h.update(name, "utf8");
	const d = h.digest();
	d[6] = (d[6] & 0x0f) | 0x50; // version 5
	d[8] = (d[8] & 0x3f) | 0x80; // variant 10
	const hex = d.subarray(0, 16).toString("hex");
	return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export function sessionUuid(oldSessionId: string): string {
	return uuidV5(MIGRATOR_NS, `pi-relay-session:${oldSessionId}`);
}

export function projectUuid(oldProjectId: string): string {
	return uuidV5(MIGRATOR_NS, `pi-relay-project:${oldProjectId}`);
}

export function sha256hex(s: string | Buffer): string {
	return createHash("sha256").update(s).digest("hex");
}

/** Per-file allocator for 8-hex entry ids derived from source entry ids.
 * Resolves the (astronomically unlikely) collision deterministically by
 * widening the hash slice. */
export class EntryIds {
	private used = new Set<string>();
	private map = new Map<string, string>();
	private salt: string;
	constructor(salt: string) {
		this.salt = salt;
	}
	id(oldId: string): string {
		const hit = this.map.get(oldId);
		if (hit) return hit;
		const full = sha256hex(`${this.salt}:${oldId}`);
		for (const len of [8, 12, 16, 20, 24, 32, 48, 64]) {
			const cand = full.slice(0, len);
			if (!this.used.has(cand)) {
				this.used.add(cand);
				this.map.set(oldId, cand);
				return cand;
			}
		}
		throw new Error("entry id space exhausted (pathological collision)");
	}
	/** A synthetic id not derived from a source entry (markers, lifecycle). */
	synthetic(label: string): string {
		return this.id(`\u0000synthetic:${label}`);
	}
}
