import type { TUI } from "@pi-relay/tui";
import stripAnsi from "strip-ansi";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { ModelSelectorComponent } from "../../../src/modes/interactive/components/model-selector.js";
import { ScopedModelsSelectorComponent } from "../../../src/modes/interactive/components/scoped-models-selector.js";
import { initTheme } from "../../../src/modes/interactive/theme/theme.js";
import { createHarness, type Harness } from "../harness.js";

function createFakeTui(): TUI {
	return {
		requestRender: () => {},
	} as unknown as TUI;
}

async function waitForAsyncRender(): Promise<void> {
	await new Promise((resolve) => setTimeout(resolve, 0));
}

describe("issue #3217 scoped model ordering", () => {
	const harnesses: Harness[] = [];

	beforeAll(() => {
		initTheme("dark");
	});

	afterEach(() => {
		while (harnesses.length > 0) {
			harnesses.pop()?.cleanup();
		}
	});

	it("propagates reordered scoped models back to the session state", async () => {
		const harness = await createHarness({
			models: [
				{ id: "faux-1", name: "One", reasoning: true },
				{ id: "faux-2", name: "Two", reasoning: true },
				{ id: "faux-3", name: "Three", reasoning: true },
			],
		});
		harnesses.push(harness);

		const orderedIds = harness.models.map((model) => `${model.provider}/${model.id}`);
		const changes: Array<string[] | null> = [];
		const selector = new ScopedModelsSelectorComponent(
			{
				allModels: [...harness.models],
				enabledModelIds: orderedIds,
			},
			{
				onChange: (enabledModelIds) => {
					changes.push(enabledModelIds);
				},
				onPersist: () => {},
				onCancel: () => {},
			},
		);

		selector.handleInput("\x1b[1;3B");

		expect(changes).toEqual([[orderedIds[1], orderedIds[0], orderedIds[2]]]);
	});

	it("preserves scoped model order in the /model scoped tab", async () => {
		const harness = await createHarness({
			models: [
				{ id: "faux-1", name: "One", reasoning: true },
				{ id: "faux-2", name: "Two", reasoning: true },
				{ id: "faux-3", name: "Three", reasoning: true },
			],
		});
		harnesses.push(harness);

		const modelOne = harness.getModel("faux-1")!;
		const modelTwo = harness.getModel("faux-2")!;
		const modelThree = harness.getModel("faux-3")!;
		const selector = new ModelSelectorComponent(
			createFakeTui(),
			modelOne,
			harness.settingsManager,
			harness.session.modelRegistry,
			[{ model: modelTwo }, { model: modelOne }, { model: modelThree }],
			() => {},
			() => {},
		);

		await waitForAsyncRender();

		const renderedLines = stripAnsi(selector.render(120).join("\n"))
			.split("\n")
			.filter((line) => line.includes(`[${modelOne.provider}]`));
		const orderedIds = renderedLines.slice(0, 3).map((line) => {
			const [modelId] = line.trim().replace(/^→\s*/, "").split(" [");
			return modelId?.trim() ?? "";
		});

		expect(orderedIds).toEqual([modelTwo.id, modelOne.id, modelThree.id]);
	});

	it("selects a custom model id when the /model search has no matches", async () => {
		const harness = await createHarness({
			models: [{ id: "faux-1", name: "One", reasoning: true }],
		});
		harnesses.push(harness);

		const currentModel = harness.getModel("faux-1")!;
		let selectedModel: typeof currentModel | undefined;
		const selector = new ModelSelectorComponent(
			createFakeTui(),
			currentModel,
			harness.settingsManager,
			harness.session.modelRegistry,
			[],
			(model) => {
				selectedModel = model;
			},
			() => {},
			"ghost-model",
		);

		await waitForAsyncRender();
		selector.handleInput("\r");

		expect(selectedModel?.provider).toBe(currentModel.provider);
		expect(selectedModel?.id).toBe("ghost-model");
		expect(harness.settingsManager.getDefaultProvider()).toBe(currentModel.provider);
		expect(harness.settingsManager.getDefaultModel()).toBe("ghost-model");
	});

	it("accepts provider-prefixed custom model ids in the /model search", async () => {
		const harness = await createHarness({
			models: [{ id: "faux-1", name: "One", reasoning: true }],
		});
		harnesses.push(harness);

		const currentModel = harness.getModel("faux-1")!;
		let selectedModel: typeof currentModel | undefined;
		const selector = new ModelSelectorComponent(
			createFakeTui(),
			currentModel,
			harness.settingsManager,
			harness.session.modelRegistry,
			[],
			(model) => {
				selectedModel = model;
			},
			() => {},
			`${currentModel.provider}/ghost-model`,
		);

		await waitForAsyncRender();
		selector.handleInput("\r");

		expect(selectedModel?.provider).toBe(currentModel.provider);
		expect(selectedModel?.id).toBe("ghost-model");
	});
});
