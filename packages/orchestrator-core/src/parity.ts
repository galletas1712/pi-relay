import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import type { OrchestratorTreeSnapshot } from "@pi-relay/agent-protocol";
import type { OrchestratorCoreCommand } from "./commands.js";
import { createEmptyOrchestratorCoreState } from "./domain/state.js";
import type { OrchestratorCoreEffect } from "./effects.js";
import { reduceOrchestratorState } from "./reducer.js";
import { toTreeSnapshot } from "./selectors.js";

export const ORCHESTRATOR_PARITY_REQUIRED_FILES = [
	"meta.json",
	"commands.ndjson",
	"expected-effects.ndjson",
	"expected-snapshot.json",
] as const;

export const ORCHESTRATOR_PARITY_OPTIONAL_FILES = ["events.ndjson", "notes.md"] as const;

export const ORCHESTRATOR_PARITY_KNOWN_FILES = [
	...ORCHESTRATOR_PARITY_REQUIRED_FILES,
	...ORCHESTRATOR_PARITY_OPTIONAL_FILES,
] as const;

const DEFAULT_PATH_FIELDS = ["sessionFile", "worklogFile"];
const TIMESTAMP_TOKEN_PATTERN = /^T\d+$/;

export interface OrchestratorParityNormalizationConfig {
	pathFields?: string[];
	timestampFields?: string[];
	stringReplacements?: Record<string, string>;
}

export interface OrchestratorParityFixtureMeta {
	id: string;
	surface: "orchestrator";
	title?: string;
	description?: string;
	source?: string;
	notes?: string;
	normalization?: OrchestratorParityNormalizationConfig;
}

export interface OrchestratorParityFixtureSummary {
	name: string;
	dir: string;
	presentFiles: string[];
	missingRequired: string[];
	replayReady: boolean;
	meta?: OrchestratorParityFixtureMeta;
}

export interface OrchestratorParityFixture<TSpawnConfig = unknown> {
	name: string;
	dir: string;
	meta: OrchestratorParityFixtureMeta;
	commands: OrchestratorCoreCommand<TSpawnConfig>[];
	events: unknown[];
	expectedEffects: OrchestratorCoreEffect<TSpawnConfig>[];
	expectedSnapshot: OrchestratorTreeSnapshot<TSpawnConfig>;
}

export interface OrchestratorParityDifference {
	path: string;
	expected: unknown;
	actual: unknown;
}

export interface OrchestratorParityReplayResult<TSpawnConfig = unknown> {
	fixtureName: string;
	dir: string;
	meta: OrchestratorParityFixtureMeta;
	commandCount: number;
	reservedEventCount: number;
	actualEffects: OrchestratorCoreEffect<TSpawnConfig>[];
	actualSnapshot: OrchestratorTreeSnapshot<TSpawnConfig>;
	normalizedExpectedEffects: unknown;
	normalizedActualEffects: unknown;
	normalizedExpectedSnapshot: unknown;
	normalizedActualSnapshot: unknown;
	effectDifferences: OrchestratorParityDifference[];
	snapshotDifferences: OrchestratorParityDifference[];
	warnings: string[];
	pass: boolean;
}

interface NormalizationState {
	pathFields: Set<string>;
	timestampFields: Set<string>;
	stringReplacements: Map<string, string>;
	timestampTokens: Map<string, string>;
}

function readJsonFile<T>(path: string): T {
	return JSON.parse(readFileSync(path, "utf8")) as T;
}

function readNdjsonFile<T>(path: string): T[] {
	if (!existsSync(path)) {
		return [];
	}

	const lines = readFileSync(path, "utf8").split(/\r?\n/);
	const entries: T[] = [];
	for (const line of lines) {
		const trimmed = line.trim();
		if (!trimmed || trimmed.startsWith("#")) {
			continue;
		}
		entries.push(JSON.parse(trimmed) as T);
	}
	return entries;
}

function createNormalizationState(config: OrchestratorParityNormalizationConfig = {}): NormalizationState {
	return {
		pathFields: new Set(config.pathFields ?? DEFAULT_PATH_FIELDS),
		timestampFields: new Set(config.timestampFields ?? []),
		stringReplacements: new Map(Object.entries(config.stringReplacements ?? {})),
		timestampTokens: new Map<string, string>(),
	};
}

function normalizePathValue(value: string): string {
	const normalized = value.replaceAll("\\", "/");
	const segments = normalized.split("/").filter(Boolean);
	return segments.at(-1) ?? value;
}

function getTimestampToken(value: string | number, state: NormalizationState): string {
	const key = String(value);
	const existing = state.timestampTokens.get(key);
	if (existing) {
		return existing;
	}
	const token = `T${state.timestampTokens.size + 1}`;
	state.timestampTokens.set(key, token);
	return token;
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function normalizeParityValueInternal(
	value: unknown,
	state: NormalizationState,
	fieldName?: string,
): unknown {
	if (typeof value === "string") {
		const replaced = state.stringReplacements.get(value) ?? value;
		if (fieldName && state.timestampFields.has(fieldName) && !TIMESTAMP_TOKEN_PATTERN.test(replaced)) {
			return getTimestampToken(replaced, state);
		}
		if (fieldName && state.pathFields.has(fieldName)) {
			return normalizePathValue(replaced);
		}
		return replaced;
	}

	if (typeof value === "number") {
		if (fieldName && state.timestampFields.has(fieldName)) {
			return getTimestampToken(value, state);
		}
		return value;
	}

	if (Array.isArray(value)) {
		return value.map((entry) => normalizeParityValueInternal(entry, state));
	}

	if (isPlainRecord(value)) {
		const normalized: Record<string, unknown> = {};
		for (const key of Object.keys(value).sort()) {
			const entry = value[key];
			const nextValue = normalizeParityValueInternal(entry, state, key);
			if (nextValue !== undefined) {
				normalized[key] = nextValue;
			}
		}
		return normalized;
	}

	return value;
}

export function normalizeOrchestratorParityValue(
	value: unknown,
	config: OrchestratorParityNormalizationConfig = {},
): unknown {
	return normalizeParityValueInternal(value, createNormalizationState(config));
}

export function listOrchestratorParityFixtures(fixtureRoot: string): string[] {
	const resolvedRoot = resolve(fixtureRoot);
	if (!existsSync(resolvedRoot)) {
		return [];
	}

	return readdirSync(resolvedRoot)
		.filter((entry) => !entry.startsWith("."))
		.filter((entry) => statSync(join(resolvedRoot, entry)).isDirectory())
		.sort();
}

export function summarizeOrchestratorParityFixture(
	fixtureRoot: string,
	fixtureName: string,
): OrchestratorParityFixtureSummary {
	const dir = join(resolve(fixtureRoot), fixtureName);
	const presentFiles = ORCHESTRATOR_PARITY_KNOWN_FILES.filter((file) => existsSync(join(dir, file)));
	const missingRequired = ORCHESTRATOR_PARITY_REQUIRED_FILES.filter((file) => !existsSync(join(dir, file)));
	const metaPath = join(dir, "meta.json");

	return {
		name: fixtureName,
		dir,
		presentFiles,
		missingRequired,
		replayReady: missingRequired.length === 0,
		meta: existsSync(metaPath) ? readJsonFile<OrchestratorParityFixtureMeta>(metaPath) : undefined,
	};
}

export function loadOrchestratorParityFixture<TSpawnConfig = unknown>(
	fixtureRoot: string,
	fixtureName: string,
): OrchestratorParityFixture<TSpawnConfig> {
	const summary = summarizeOrchestratorParityFixture(fixtureRoot, fixtureName);
	if (summary.missingRequired.length > 0) {
		throw new Error(
			`Fixture '${fixtureName}' is missing required files: ${summary.missingRequired.join(", ")}`,
		);
	}
	if (!summary.meta) {
		throw new Error(`Fixture '${fixtureName}' is missing meta.json`);
	}
	if (summary.meta.surface !== "orchestrator") {
		throw new Error(`Fixture '${fixtureName}' has unsupported surface '${String(summary.meta.surface)}'`);
	}

	return {
		name: summary.name,
		dir: summary.dir,
		meta: summary.meta,
		commands: readNdjsonFile<OrchestratorCoreCommand<TSpawnConfig>>(join(summary.dir, "commands.ndjson")),
		events: readNdjsonFile(join(summary.dir, "events.ndjson")),
		expectedEffects: readNdjsonFile<OrchestratorCoreEffect<TSpawnConfig>>(join(summary.dir, "expected-effects.ndjson")),
		expectedSnapshot: readJsonFile<OrchestratorTreeSnapshot<TSpawnConfig>>(join(summary.dir, "expected-snapshot.json")),
	};
}

export function collectOrchestratorParityDifferences(
	expected: unknown,
	actual: unknown,
	path = "$",
	limit = 20,
): OrchestratorParityDifference[] {
	const differences: OrchestratorParityDifference[] = [];

	const visit = (expectedValue: unknown, actualValue: unknown, currentPath: string) => {
		if (differences.length >= limit) {
			return;
		}

		if (Object.is(expectedValue, actualValue)) {
			return;
		}

		if (Array.isArray(expectedValue) && Array.isArray(actualValue)) {
			if (expectedValue.length !== actualValue.length) {
				differences.push({
					path: `${currentPath}.length`,
					expected: expectedValue.length,
					actual: actualValue.length,
				});
				if (differences.length >= limit) {
					return;
				}
			}
			const maxLength = Math.max(expectedValue.length, actualValue.length);
			for (let index = 0; index < maxLength; index += 1) {
				visit(expectedValue[index], actualValue[index], `${currentPath}[${index}]`);
				if (differences.length >= limit) {
					return;
				}
			}
			return;
		}

		if (isPlainRecord(expectedValue) && isPlainRecord(actualValue)) {
			const keys = new Set([...Object.keys(expectedValue), ...Object.keys(actualValue)]);
			for (const key of [...keys].sort()) {
				visit(expectedValue[key], actualValue[key], `${currentPath}.${key}`);
				if (differences.length >= limit) {
					return;
				}
			}
			return;
		}

		differences.push({
			path: currentPath,
			expected: expectedValue,
			actual: actualValue,
		});
	};

	visit(expected, actual, path);
	return differences;
}

function describeDifferenceValue(value: unknown): string {
	if (value === undefined) {
		return "<missing>";
	}
	const serialized = JSON.stringify(value);
	return serialized ?? String(value);
}

export function formatOrchestratorParityDifference(difference: OrchestratorParityDifference): string {
	return `${difference.path}: expected ${describeDifferenceValue(difference.expected)}, received ${describeDifferenceValue(difference.actual)}`;
}

export function replayOrchestratorParityFixture<TSpawnConfig = unknown>(
	fixture: OrchestratorParityFixture<TSpawnConfig>,
): OrchestratorParityReplayResult<TSpawnConfig> {
	let state = createEmptyOrchestratorCoreState<TSpawnConfig>();
	const actualEffects: OrchestratorCoreEffect<TSpawnConfig>[] = [];

	for (const command of fixture.commands) {
		const result = reduceOrchestratorState(state, command);
		state = result.state;
		actualEffects.push(...result.effects);
	}

	const actualSnapshot = toTreeSnapshot(state);
	const normalization = fixture.meta.normalization ?? {};
	const normalizedExpectedEffects = normalizeOrchestratorParityValue(fixture.expectedEffects, normalization);
	const normalizedActualEffects = normalizeOrchestratorParityValue(actualEffects, normalization);
	const normalizedExpectedSnapshot = normalizeOrchestratorParityValue(fixture.expectedSnapshot, normalization);
	const normalizedActualSnapshot = normalizeOrchestratorParityValue(actualSnapshot, normalization);
	const effectDifferences = collectOrchestratorParityDifferences(
		normalizedExpectedEffects,
		normalizedActualEffects,
		"$.effects",
	);
	const snapshotDifferences = collectOrchestratorParityDifferences(
		normalizedExpectedSnapshot,
		normalizedActualSnapshot,
		"$.snapshot",
	);
	const warnings =
		fixture.events.length > 0
			? [
				`Fixture '${fixture.name}' includes ${fixture.events.length} reserved host events. Phase 2 replay currently replays commands only.`,
			]
			: [];

	return {
		fixtureName: fixture.name,
		dir: fixture.dir,
		meta: fixture.meta,
		commandCount: fixture.commands.length,
		reservedEventCount: fixture.events.length,
		actualEffects,
		actualSnapshot,
		normalizedExpectedEffects,
		normalizedActualEffects,
		normalizedExpectedSnapshot,
		normalizedActualSnapshot,
		effectDifferences,
		snapshotDifferences,
		warnings,
		pass: effectDifferences.length === 0 && snapshotDifferences.length === 0,
	};
}
