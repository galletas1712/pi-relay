import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import process from "node:process";

const SURFACE = "session";
const DEFAULT_ROOT = `testdata/parity/${SURFACE}`;
const REQUIRED_FILES = ["meta.json"];
const KNOWN_FILES = [
	"meta.json",
	"commands.ndjson",
	"events.ndjson",
	"expected-effects.ndjson",
	"expected-snapshot.json",
	"notes.md",
];

function printUsage() {
	console.log(`Usage: node scripts/parity/${SURFACE}-replay.mjs [fixture-root] [--fixture NAME] [--json]\n\nPhase 0 behavior: discover fixtures, validate the baseline layout, and print a scaffold summary. Replay execution is intentionally not wired yet.`);
}

function parseArgs(argv) {
	const args = [...argv];
	let fixtureRoot = DEFAULT_ROOT;
	let fixtureName;
	let json = false;

	while (args.length > 0) {
		const arg = args.shift();
		if (!arg) continue;
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

function listFixtureDirectories(fixtureRoot) {
	if (!existsSync(fixtureRoot)) {
		return [];
	}

	return readdirSync(fixtureRoot)
		.filter((entry) => !entry.startsWith("."))
		.filter((entry) => statSync(join(fixtureRoot, entry)).isDirectory())
		.sort();
}

function readMeta(metaPath) {
	if (!existsSync(metaPath)) {
		return undefined;
	}

	const raw = readFileSync(metaPath, "utf8");
	return JSON.parse(raw);
}

function summarizeFixture(fixtureRoot, fixtureName) {
	const fixtureDir = join(fixtureRoot, fixtureName);
	const presentFiles = KNOWN_FILES.filter((file) => existsSync(join(fixtureDir, file)));
	const missingRequired = REQUIRED_FILES.filter((file) => !existsSync(join(fixtureDir, file)));
	const meta = readMeta(join(fixtureDir, "meta.json"));

	return {
		name: fixtureName,
		dir: fixtureDir,
		status: missingRequired.length === 0 ? "scaffold-ready" : "missing-required-files",
		missingRequired,
		presentFiles,
		meta,
	};
}

function printSummary(result) {
	console.log(`Surface: ${SURFACE}`);
	console.log(`Fixture root: ${result.fixtureRoot}`);
	if (!result.fixtures.length) {
		console.log(
			`No ${SURFACE} parity fixtures found. Add ${SURFACE}/<fixture>/meta.json under testdata/parity/ to start capturing replay traces.`,
		);
		return;
	}

	for (const fixture of result.fixtures) {
		console.log(`\n- ${fixture.name} [${fixture.status}]`);
		console.log(`  dir: ${fixture.dir}`);
		console.log(`  files: ${fixture.presentFiles.join(", ") || "(none yet)"}`);
		if (fixture.missingRequired.length) {
			console.log(`  missing required: ${fixture.missingRequired.join(", ")}`);
		}
		if (fixture.meta) {
			const label = fixture.meta.title || fixture.meta.description || fixture.meta.id || basename(fixture.dir);
			console.log(`  meta: ${label}`);
		}
	}

	console.log("\nPhase 0 scaffold only: replay execution and diffing are added in later milestones.");
}

try {
	const options = parseArgs(process.argv.slice(2));
	const fixtureNames = options.fixtureName
		? [options.fixtureName]
		: listFixtureDirectories(options.fixtureRoot);

	if (options.fixtureName && fixtureNames.length === 1 && !existsSync(join(options.fixtureRoot, options.fixtureName))) {
		throw new Error(`Fixture '${options.fixtureName}' was not found under ${options.fixtureRoot}`);
	}

	const fixtures = fixtureNames.map((fixtureName) => summarizeFixture(options.fixtureRoot, fixtureName));
	const hasErrors = fixtures.some((fixture) => fixture.status !== "scaffold-ready");
	const result = {
		surface: SURFACE,
		fixtureRoot: options.fixtureRoot,
		fixtureCount: fixtures.length,
		fixtures,
	};

	if (options.json) {
		console.log(JSON.stringify(result, null, 2));
	} else {
		printSummary(result);
	}

	process.exit(hasErrors ? 1 : 0);
} catch (error) {
	console.error(error instanceof Error ? error.message : String(error));
	process.exit(1);
}
