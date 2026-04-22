import { existsSync } from "node:fs";
import { resolve } from "node:path";
import process from "node:process";
import {
	formatOrchestratorParityDifference,
	listOrchestratorParityFixtures,
	loadOrchestratorParityFixture,
	replayOrchestratorParityFixture,
	summarizeOrchestratorParityFixture,
} from "../../packages/orchestrator-core/src/parity.ts";

const SURFACE = "orchestrator";
const DEFAULT_ROOT = `testdata/parity/${SURFACE}`;

interface ReplayCliOptions {
	fixtureRoot: string;
	fixtureName?: string;
	json: boolean;
}

interface ReplayFixtureSummary {
	name: string;
	dir: string;
	status: "pass" | "fail" | "missing-replay-files" | "error";
	presentFiles: string[];
	missingRequired: string[];
	meta?: unknown;
	commandCount?: number;
	reservedEventCount?: number;
	effectCount?: number;
	warningCount?: number;
	warnings?: string[];
	effectDifferences?: string[];
	snapshotDifferences?: string[];
	error?: string;
}

function printUsage() {
	console.log(
		`Usage: node scripts/parity/${SURFACE}-replay.mjs [fixture-root] [--fixture NAME] [--json]\n\nPhase 2 behavior: load orchestrator fixtures from testdata/parity/, replay them against the extracted TypeScript core, normalize unstable fields, and report actionable diffs.`,
	);
}

function parseArgs(argv: string[]): ReplayCliOptions {
	const args = [...argv];
	let fixtureRoot = DEFAULT_ROOT;
	let fixtureName: string | undefined;
	let json = false;

	while (args.length > 0) {
		const arg = args.shift();
		if (!arg) {
			continue;
		}
		if (arg === "--help" || arg === "-h") {
			printUsage();
			process.exit(0);
		}
		if (arg === "--json") {
			json = true;
			continue;
		}
		if (arg === "--fixture") {
			fixtureName = args.shift();
			if (!fixtureName) {
				throw new Error("--fixture requires a fixture name");
			}
			continue;
		}
		fixtureRoot = arg;
	}

	return {
		fixtureRoot: resolve(fixtureRoot),
		fixtureName,
		json,
	};
}

function summarizeReplayOutcome(fixtureRoot: string, fixtureName: string): ReplayFixtureSummary {
	const fixtureSummary = summarizeOrchestratorParityFixture(fixtureRoot, fixtureName);
	if (!fixtureSummary.replayReady) {
		return {
			name: fixtureSummary.name,
			dir: fixtureSummary.dir,
			status: "missing-replay-files",
			presentFiles: fixtureSummary.presentFiles,
			missingRequired: fixtureSummary.missingRequired,
			meta: fixtureSummary.meta,
		};
	}

	const fixture = loadOrchestratorParityFixture(fixtureRoot, fixtureName);
	const replay = replayOrchestratorParityFixture(fixture);
	return {
		name: fixtureSummary.name,
		dir: fixtureSummary.dir,
		status: replay.pass ? "pass" : "fail",
		presentFiles: fixtureSummary.presentFiles,
		missingRequired: [],
		meta: fixture.meta,
		commandCount: replay.commandCount,
		reservedEventCount: replay.reservedEventCount,
		effectCount: replay.actualEffects.length,
		warningCount: replay.warnings.length,
		warnings: replay.warnings,
		effectDifferences: replay.effectDifferences.map(formatOrchestratorParityDifference),
		snapshotDifferences: replay.snapshotDifferences.map(formatOrchestratorParityDifference),
	};
}

function printSummary(fixtureRoot: string, fixtures: ReplayFixtureSummary[]) {
	console.log(`Surface: ${SURFACE}`);
	console.log(`Fixture root: ${fixtureRoot}`);
	if (fixtures.length === 0) {
		console.log(`No ${SURFACE} parity fixtures found under ${fixtureRoot}.`);
		return;
	}

	for (const fixture of fixtures) {
		const label =
			typeof fixture.meta === "object" && fixture.meta !== null && "title" in fixture.meta
				? String((fixture.meta as { title?: string }).title ?? fixture.name)
				: fixture.name;
		console.log(`\n- ${fixture.name} [${fixture.status}]`);
		console.log(`  title: ${label}`);
		console.log(`  dir: ${fixture.dir}`);
		console.log(`  files: ${fixture.presentFiles.join(", ") || "(none)"}`);
		if (fixture.missingRequired.length > 0) {
			console.log(`  missing required: ${fixture.missingRequired.join(", ")}`);
		}
		if (fixture.commandCount !== undefined) {
			console.log(`  commands: ${fixture.commandCount}`);
			console.log(`  effects: ${fixture.effectCount ?? 0}`);
			if (fixture.reservedEventCount) {
				console.log(`  reserved host events: ${fixture.reservedEventCount}`);
			}
		}
		for (const warning of fixture.warnings ?? []) {
			console.log(`  warning: ${warning}`);
		}
		for (const difference of [...(fixture.effectDifferences ?? []), ...(fixture.snapshotDifferences ?? [])].slice(0, 8)) {
			console.log(`  diff: ${difference}`);
		}
		if (fixture.error) {
			console.log(`  error: ${fixture.error}`);
		}
	}

	const passCount = fixtures.filter((fixture) => fixture.status === "pass").length;
	console.log(`\nPassed ${passCount}/${fixtures.length} ${SURFACE} parity fixture(s).`);
}

try {
	const options = parseArgs(process.argv.slice(2));
	const fixtureNames = options.fixtureName
		? [options.fixtureName]
		: listOrchestratorParityFixtures(options.fixtureRoot);

	if (options.fixtureName && !existsSync(resolve(options.fixtureRoot, options.fixtureName))) {
		throw new Error(`Fixture '${options.fixtureName}' was not found under ${options.fixtureRoot}`);
	}

	const fixtures = fixtureNames.map((fixtureName) => {
		try {
			return summarizeReplayOutcome(options.fixtureRoot, fixtureName);
		} catch (error) {
			const summary = summarizeOrchestratorParityFixture(options.fixtureRoot, fixtureName);
			return {
				name: fixtureName,
				dir: summary.dir,
				status: "error" as const,
				presentFiles: summary.presentFiles,
				missingRequired: summary.missingRequired,
				meta: summary.meta,
				error: error instanceof Error ? error.message : String(error),
			};
		}
	});

	const result = {
		surface: SURFACE,
		fixtureRoot: options.fixtureRoot,
		fixtureCount: fixtures.length,
		fixtures,
	};

	if (options.json) {
		console.log(JSON.stringify(result, null, 2));
	} else {
		printSummary(options.fixtureRoot, fixtures);
	}

	const hasFailures = fixtures.some((fixture) => fixture.status !== "pass");
	process.exit(hasFailures ? 1 : 0);
} catch (error) {
	console.error(error instanceof Error ? error.message : String(error));
	process.exit(1);
}
