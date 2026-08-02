import { describe, expect, it } from "vitest";
import { navigationPolicy, parseAppUrl } from "./policy.mjs";

describe("parseAppUrl", () => {
	it("accepts HTTP(S) app URLs", () => {
		expect(parseAppUrl(" https://pi-relay.example/app ")).toEqual(
			new URL("https://pi-relay.example/app"),
		);
		expect(parseAppUrl("http://127.0.0.1:8788")).toEqual(
			new URL("http://127.0.0.1:8788"),
		);
	});

	it.each([
		"",
		"not a URL",
		"ftp://example.com",
		"https://user@example.com",
		"https://:secret@example.com",
	])(
		"rejects invalid or unsafe app URL %s",
		(value) => {
			expect(() => parseAppUrl(value)).toThrow();
		},
	);
});

describe("navigationPolicy", () => {
	const appOrigin = "https://pi-relay.example";

	it("allows same-origin HTTP(S) navigation", () => {
		expect(navigationPolicy("https://pi-relay.example/session/1", appOrigin)).toEqual({
			action: "allow",
		});
	});

	it("opens cross-origin HTTP(S) navigation externally", () => {
		expect(navigationPolicy("https://login.example/oauth", appOrigin)).toEqual({
			action: "external",
			url: "https://login.example/oauth",
		});
	});

	it.each([
		"javascript:alert(document.cookie)",
		"data:text/html,<script>alert(1)</script>",
		"https://user:password@pi-relay.example/private",
		"mailto:user@example.com",
	])("denies unsafe navigation %s", (url) => {
		expect(navigationPolicy(url, appOrigin)).toEqual({ action: "deny" });
	});
});
