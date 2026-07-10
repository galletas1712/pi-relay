// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MAX_MCP_CALLBACK_URL_LENGTH, McpOAuthDialog } from "./mcpOAuthDialog.tsx";

const LOGIN = {
	login_id: "0000000000000001",
	authorization_url:
		"https://auth.example.test/authorize?state=secret-state&code_challenge=secret-verifier",
	expires_at_unix_seconds: 1_900_000_000,
};

beforeEach(() => {
	Object.defineProperty(navigator, "clipboard", {
		configurable: true,
		value: { writeText: vi.fn(async () => {}) },
	});
});

afterEach(() => {
	cleanup();
	vi.restoreAllMocks();
});

describe("McpOAuthDialog", () => {
	it("offers an explicit safe authorization link and copy fallback without storage", async () => {
		const localSet = vi.spyOn(window.localStorage, "setItem");
		const sessionSet = vi.spyOn(window.sessionStorage, "setItem");
		const onCancel = vi.fn(async () => {});
		render(
			<McpOAuthDialog
				server="remote"
				login={LOGIN}
				onComplete={async () => {}}
				onCancel={onCancel}
			/>,
		);
		const link = screen.getByRole("link", { name: "Open authorization page" });
		expect(link.getAttribute("href")).toBe(LOGIN.authorization_url);
		expect(link.getAttribute("target")).toBe("_blank");
		expect(link.getAttribute("rel")).toBe("noopener noreferrer");
		await waitFor(() => expect(document.activeElement).toBe(link));
		expect(screen.getByText(/daemon is listening.*loopback/i)).toBeTruthy();
		await userEvent.click(screen.getByRole("button", { name: "Copy" }));
		expect(navigator.clipboard.writeText).toHaveBeenCalledWith(LOGIN.authorization_url);
		await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
		await waitFor(() => expect(onCancel).toHaveBeenCalled());
		expect(localSet).not.toHaveBeenCalled();
		expect(sessionSet).not.toHaveBeenCalled();
	});

	it("disables every mutation path while the connection is unavailable", () => {
		render(
			<McpOAuthDialog
				server="remote"
				login={LOGIN}
				onComplete={async () => {}}
				onCancel={async () => {}}
				mutationBlockedReason="Waiting for connection"
			/>,
		);
		expect(screen.getByRole<HTMLButtonElement>("button", { name: "Cancel" }).disabled).toBe(true);
		expect(screen.getByRole<HTMLButtonElement>("button", { name: "Complete" }).disabled).toBe(true);
		expect(screen.getByRole("status").textContent).toContain("Waiting for connection");
	});

	it("submits the entire callback URL and enforces the browser-side bound", async () => {
		const onComplete = vi.fn(async () => {});
		const callbackUrl =
			"http://127.0.0.1:43123/oauth/callback/0000000000000001?code=code&state=state";
		render(
			<McpOAuthDialog
				server="remote"
				login={LOGIN}
				onComplete={onComplete}
				onCancel={async () => {}}
			/>,
		);
		const input = screen.getByLabelText("Callback URL for a remote daemon");
		expect(input.getAttribute("maxlength")).toBe(String(MAX_MCP_CALLBACK_URL_LENGTH));
		await userEvent.type(input, callbackUrl);
		await userEvent.click(screen.getByRole("button", { name: "Complete" }));
		expect(onComplete).toHaveBeenCalledWith(callbackUrl);
	});

	it("cancels from the explicit action and reports recoverable action errors", async () => {
		const onCancel = vi.fn()
			.mockRejectedValueOnce(new Error("mcp_oauth_login_failed: fixed failure"))
			.mockResolvedValueOnce(undefined);
		render(
			<McpOAuthDialog
				server="remote"
				login={LOGIN}
				onComplete={async () => {
					throw new Error("mcp_oauth_callback_invalid: fixed callback failure");
				}}
				onCancel={onCancel}
			/>,
		);
		await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
		expect((await screen.findByRole("alert")).textContent).toContain("fixed failure");
		const callbackUrl =
			"http://127.0.0.1:43123/oauth/callback/0000000000000001?code=bad&state=bad";
		await userEvent.type(screen.getByLabelText("Callback URL for a remote daemon"), callbackUrl);
		await userEvent.click(screen.getByRole("button", { name: "Complete" }));
		expect((await screen.findByRole("alert")).textContent).toContain("fixed callback failure");
		await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
		await waitFor(() => expect(onCancel).toHaveBeenCalledTimes(2));
	});
});
