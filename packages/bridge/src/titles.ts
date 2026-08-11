// Sidecar session titles (M8, G1): when a session settles to idle and still
// has no name, ask the title model (GLM via the m1-demo SSE shim) for a short
// title from the first user message, then run the normal rename path
// (PG update + session.renamed + best-effort host set_session_name).
// Direct non-streaming HTTP; the API key comes only from the process env.
import { readFileSync } from "node:fs";
import { config } from "./config.ts";

/** First user-message text in a pi session JSONL (v3), for title generation. */
export function firstUserText(sessionFile: string | null): string | null {
	if (!sessionFile) return null;
	let raw: string;
	try {
		raw = readFileSync(sessionFile, "utf8");
	} catch {
		return null;
	}
	for (const line of raw.split("\n")) {
		if (!line.includes('"role":"user"')) continue; // cheap prefilter
		try {
			const rec = JSON.parse(line);
			if (rec?.type !== "message" || rec.message?.role !== "user") continue;
			const content = Array.isArray(rec.message.content) ? rec.message.content : [];
			const text = content
				.filter((c: { type?: string }) => c?.type === "text")
				.map((c: { text?: string }) => c.text ?? "")
				.join("\n")
				.trim();
			if (text) return text;
		} catch {
			continue;
		}
	}
	return null;
}

const TITLE_SYSTEM =
	"Generate a very short title (3-7 words, no punctuation at the end, no quotes) " +
	"for a chat that starts with the following user message. Reply with ONLY the title.";

/** Ask the shim for a title; null on any failure (titles are best-effort). */
export async function generateTitle(userText: string): Promise<string | null> {
	const apiKey = process.env[config.titleApiKeyEnv];
	if (!apiKey) return null;
	try {
		const resp = await fetch(`${config.titleShimBaseUrl}/chat/completions`, {
			method: "POST",
			headers: { "content-type": "application/json", authorization: `Bearer ${apiKey}` },
			body: JSON.stringify({
				model: config.titleModel,
				stream: false,
				max_tokens: 300, // GLM reasoning burns tokens before content; leave headroom
				messages: [
					{ role: "system", content: TITLE_SYSTEM },
					{ role: "user", content: userText.slice(0, 2000) },
				],
			}),
			signal: AbortSignal.timeout(45_000),
		});
		if (!resp.ok) return null;
		const data = (await resp.json()) as { choices?: Array<{ message?: { content?: string } }> };
		const rawTitle = data.choices?.[0]?.message?.content ?? "";
		const clean = rawTitle
			.split("\n")[0]
			.replace(/^[\s"'`*#]+|[\s"'`*#.!?]+$/g, "")
			.replace(/\s+/g, " ")
			.trim()
			.slice(0, 80);
		return clean.length >= 3 ? clean : null;
	} catch {
		return null;
	}
}
