import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
	formatOrchestratorParityDifference,
	loadOrchestratorParityFixture,
	normalizeOrchestratorParityValue,
	replayOrchestratorParityFixture,
} from "../src/index.js";

const FIXTURE_ROOT = resolve(import.meta.dirname, "../../../testdata/parity/orchestrator");

describe("orchestrator-core parity replay", () => {
	it("replays the sample fixture and matches normalized effects and snapshots", () => {
		const fixture = loadOrchestratorParityFixture(FIXTURE_ROOT, "root-child-lifecycle");
		const replay = replayOrchestratorParityFixture(fixture);

		expect(replay.pass).toBe(true);
		expect(replay.commandCount).toBe(5);
		expect(replay.actualEffects).toHaveLength(10);
		expect(replay.normalizedActualEffects).toEqual(replay.normalizedExpectedEffects);
		expect(replay.normalizedActualSnapshot).toEqual(replay.normalizedExpectedSnapshot);
		expect(replay.normalizedActualSnapshot).toMatchObject({
			agents: {
				root: {
					sessionFile: "2026-04-22T04-00-00_root-session.jsonl",
					worklogFile: "root.worklog.md",
				},
				"child-a": {
					sessionFile: "2026-04-22T04-00-05_child-a.jsonl",
					worklogFile: "child-a.worklog.md",
				},
			},
		});
	});

	it("normalizes path basenames, timestamp tokens, and exact string replacements", () => {
		const normalized = normalizeOrchestratorParityValue(
			{
				agent: {
					createdAt: 1700000000000,
					child: {
						lastStatusChange: 1700000001000,
					},
					sessionFile: "/tmp/pi-relay/sessions/root-session.jsonl",
					worklogFile: "C:\\tmp\\pi-relay\\worklogs\\root.worklog.md",
					status: "running",
				},
			},
			{
				pathFields: ["sessionFile", "worklogFile"],
				timestampFields: ["createdAt", "lastStatusChange"],
				stringReplacements: { running: "ACTIVE" },
			},
		);

		expect(normalized).toEqual({
			agent: {
				child: {
					lastStatusChange: "T1",
				},
				createdAt: "T2",
				sessionFile: "root-session.jsonl",
				status: "ACTIVE",
				worklogFile: "root.worklog.md",
			},
		});
	});

	it("assigns timestamp tokens deterministically regardless of object insertion order", () => {
		const config = {
			timestampFields: ["createdAt", "lastStatusChange", "updatedAt"],
		};
		const first = {
			agent: {
				updatedAt: 30,
				createdAt: 10,
				child: {
					lastStatusChange: 20,
				},
			},
		};
		const second = {
			agent: {
				child: {
					lastStatusChange: 20,
				},
				createdAt: 10,
				updatedAt: 30,
			},
		};

		expect(normalizeOrchestratorParityValue(first, config)).toEqual({
			agent: {
				child: {
					lastStatusChange: "T1",
				},
				createdAt: "T2",
				updatedAt: "T3",
			},
		});
		expect(normalizeOrchestratorParityValue(first, config)).toEqual(
			normalizeOrchestratorParityValue(second, config),
		);
	});

	it("surfaces actionable diffs when replay output diverges", () => {
		const fixture = loadOrchestratorParityFixture(FIXTURE_ROOT, "root-child-lifecycle");
		const replay = replayOrchestratorParityFixture({
			...fixture,
			expectedSnapshot: {
				...fixture.expectedSnapshot,
				agents: {
					...fixture.expectedSnapshot.agents,
					root: {
						...fixture.expectedSnapshot.agents.root,
						role: "coordinator",
					},
				},
			},
		});

		expect(replay.pass).toBe(false);
		expect(replay.snapshotDifferences[0]).toMatchObject({
			path: "$.snapshot.agents.root.role",
			expected: "coordinator",
			actual: "root",
		});
		expect(formatOrchestratorParityDifference(replay.snapshotDifferences[0]!)).toContain(
			"$.snapshot.agents.root.role",
		);
	});
});
