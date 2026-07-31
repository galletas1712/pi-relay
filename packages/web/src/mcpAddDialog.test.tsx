// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { McpAddDialog } from "./mcpAddDialog.tsx";

afterEach(cleanup);

describe("McpAddDialog", () => {
	it("shows inventory errors and retries", async () => {
		const onRetry = vi.fn();
		render(
			<McpAddDialog
				inventory={null}
				selection={new Map()}
				lockedSelection={new Map()}
				loading={false}
				error="runtime is offline"
				onChange={vi.fn()}
				onRetry={onRetry}
				onClose={vi.fn()}
				onSubmit={vi.fn()}
			/>,
		);

		expect(screen.getByRole("alert").textContent).toContain("runtime is offline");
		await userEvent.click(screen.getByRole("button", { name: "Retry" }));
		expect(onRetry).toHaveBeenCalledOnce();
	});
});
