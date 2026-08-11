// Minimal test client for the bridge contract v0. Each scenario script drives
// one or more connections and records every frame to a trace JSONL.
import WebSocket from "ws";
import { createWriteStream, mkdirSync } from "node:fs";
import { dirname } from "node:path";

export class BridgeClient {
	constructor(url, token, origin, tracePath) {
		this.url = url;
		this.token = token;
		this.origin = origin;
		this.tracePath = tracePath;
		this.pending = new Map();
		this.events = [];
		this.reqCounter = 0;
		this.waiters = [];
		this.closed = false;
		this.closeCode = null;
	}

	connect() {
		mkdirSync(dirname(this.tracePath), { recursive: true });
		this.trace = createWriteStream(this.tracePath, { flags: "a" });
		this.ws = new WebSocket(this.url, {
			headers: { Authorization: `Bearer ${this.token}`, Origin: this.origin },
			maxPayload: 16 * 1024 * 1024,
		});
		this.ws.on("message", (data) => {
			const line = data.toString("utf8");
			this.log("recv", line);
			let obj;
			try { obj = JSON.parse(line); } catch { return; }
			if (obj.id !== undefined && obj.id !== null && this.pending.has(String(obj.id))) {
				const p = this.pending.get(String(obj.id));
				this.pending.delete(String(obj.id));
				p(obj);
				return;
			}
			if (obj.event) {
				this.events.push(obj);
				for (const w of [...this.waiters]) {
					if (w.pred(obj)) {
						this.waiters.splice(this.waiters.indexOf(w), 1);
						w.resolve(obj);
					}
				}
			}
		});
		return new Promise((resolve, reject) => {
			this.ws.on("open", () => { this.log("open", ""); resolve(this); });
			this.ws.on("error", reject);
			this.ws.on("close", (code) => { this.closed = true; this.closeCode = code; this.log("close", String(code)); });
		});
	}

	log(dir, line) {
		this.trace.write(`# ${dir} ${new Date().toISOString()} ${line}\n`);
	}

	call(method, params = {}) {
		const id = `t${++this.reqCounter}`;
		const obj = { id, method, params };
		this.log("send", JSON.stringify(obj));
		return new Promise((resolve) => {
			this.pending.set(id, resolve);
			this.ws.send(JSON.stringify(obj));
		});
	}

	/** Wait for an event matching pred; resolves with the event. */
	waitEvent(pred, timeoutMs = 60000, what = "event") {
		// check already-received events first
		for (const ev of this.events) {
			if (pred(ev)) return Promise.resolve(ev);
		}
		return new Promise((resolve, reject) => {
			const timer = setTimeout(() => reject(new Error(`timeout waiting for ${what}`)), timeoutMs);
			this.waiters.push({
				pred,
				resolve: (ev) => { clearTimeout(timer); resolve(ev); },
			});
		});
	}

	/** Wait for predicate over the growing event list, scanning new arrivals. */
	async waitForEvents(pred, timeoutMs, what) {
		return this.waitEvent(pred, timeoutMs, what);
	}

	async waitSettled(sessionId, timeoutMs = 600000) {
		return this.waitEvent(
			(ev) => ev.event === "session.state" && ev.sessionId === sessionId && ev.data?.state === "idle",
			timeoutMs,
			"session idle",
		);
	}

	/** Idle event strictly after seq `afterSeq` (avoids matching pre-prompt idles). */
	async waitIdleAfter(sessionId, afterSeq, timeoutMs = 600000) {
		return this.waitEvent(
			(ev) => ev.event === "session.state" && ev.sessionId === sessionId && ev.data?.state === "idle" && ev.seq > afterSeq,
			timeoutMs,
			`idle after seq ${afterSeq}`,
		);
	}

	headSeq(sessionId) {
		let h = 0;
		for (const ev of this.events) if (ev.sessionId === sessionId && ev.seq > h) h = ev.seq;
		return h;
	}

	close() {
		try { this.ws?.close(); } catch {}
		this.trace?.end();
	}
}

export function collectText(events, sessionId) {
	return events
		.filter((ev) => ev.event === "message.delta" && ev.sessionId === sessionId && ev.data?.kind === "text")
		.map((ev) => ev.data.delta ?? "")
		.join("");
}

export function assert(cond, msg) {
	if (!cond) throw new Error(`ASSERT FAILED: ${msg}`);
	console.log(`  ✓ ${msg}`);
}
