// Durable JSONL outbox for prime-comms (M2).
//
// Every send appends a "queued" record BEFORE attempting delivery, then a
// terminal record ("delivered"/"failed"/"persisted"/"recovered") after. On
// session_start the extension folds the file and re-drives records whose last
// status is "queued" — so a host crash mid-delivery (C2) never silently drops
// a message.

import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { dirname } from "node:path";
import type { OutboxRecord } from "./protocol.ts";

export function appendOutboxRecord(outboxPath: string, record: OutboxRecord): void {
	mkdirSync(dirname(outboxPath), { recursive: true });
	appendFileSync(outboxPath, `${JSON.stringify(record)}\n`, "utf8");
}

export function readOutbox(outboxPath: string): OutboxRecord[] {
	if (!existsSync(outboxPath)) return [];
	const records: OutboxRecord[] = [];
	for (const line of readFileSync(outboxPath, "utf8").split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		try {
			const parsed = JSON.parse(trimmed) as OutboxRecord;
			if (typeof parsed.id === "string" && typeof parsed.status === "string") {
				records.push(parsed);
			}
		} catch {
			// skip torn tail lines (crash mid-append)
		}
	}
	return records;
}

/** Records whose latest status is still "queued" (never terminally resolved). */
export function pendingOutboxRecords(outboxPath: string): OutboxRecord[] {
	const lastStatus = new Map<string, OutboxRecord>();
	for (const record of readOutbox(outboxPath)) {
		lastStatus.set(record.id, record);
	}
	return [...lastStatus.values()].filter((record) => record.status === "queued");
}
