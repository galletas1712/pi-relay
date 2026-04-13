import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import process from "node:process";
import { getModel } from "../pi-mono/packages/ai/dist/index.js";
import { SessionManager } from "../pi-mono/packages/coding-agent/dist/index.js";
import { createRelayRuntime } from "../packages/app/dist/runtime.js";

const repoDir = "/home/schwinns/pi-relay-phase1";
const outputDir = "/tmp/pi-relay-phase23-e2e";
const sessionDir = join(outputDir, "session");
const reportPath = join(outputDir, "report.json");

rmSync(reportPath, { force: true });
rmSync(sessionDir, { recursive: true, force: true });
mkdirSync(sessionDir, { recursive: true });

const report = {
	success: false,
	repoDir,
	sessionDir,
	checks: {},
	notes: [],
};

function saveReport() {
	writeFileSync(reportPath, JSON.stringify(report, null, 2), "utf8");
}

function sleep(ms) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(predicate, timeoutMs, intervalMs, label) {
	const start = Date.now();
	while (Date.now() - start < timeoutMs) {
		const value = await predicate();
		if (value) {
			return value;
		}
		await sleep(intervalMs);
	}
	throw new Error(`Timed out waiting for ${label}`);
}

function readTree(treeFile) {
	return JSON.parse(readFileSync(treeFile, "utf8"));
}

function extractTextContent(content) {
	if (typeof content === "string") {
		return content;
	}
	if (!Array.isArray(content)) {
		return "";
	}
	return content
		.filter((block) => block.type === "text")
		.map((block) => block.text)
		.join("\n");
}

function timestampMs(value) {
	if (typeof value === "number") {
		return value;
	}
	if (typeof value === "string") {
		const parsed = Date.parse(value);
		return Number.isNaN(parsed) ? 0 : parsed;
	}
	return 0;
}

try {
	const sessionManager = SessionManager.create(repoDir, sessionDir);
	const runtime = await createRelayRuntime({ cwd: repoDir, sessionManager });
	await runtime.session.bindExtensions({});

	const model = getModel("openai-codex", "gpt-5.4");
	if (!model) {
		throw new Error("openai-codex/gpt-5.4 model not available");
	}

	await runtime.session.setModel(model);
	runtime.session.setThinkingLevel("xhigh");

	report.sessionId = runtime.session.sessionId;
	report.model = `${model.provider}/${model.id}`;

	const workspaceDir = join(sessionDir, runtime.session.sessionId);
	const treeFile = join(workspaceDir, "tree.json");
	const rootWorklogFile = join(workspaceDir, "worklogs", "root.worklog.md");

	function customMessages(customType) {
		return runtime.session.agent.state.messages.filter(
			(message) => message.role === "custom" && message.customType === customType,
		);
	}

	function pendingBashResults() {
		return runtime.session.agent.state.messages.filter(
			(message) =>
				message.role === "toolResult" &&
				message.toolName === "bash" &&
				message.content.some((block) => block.type === "text" && block.text.includes("[PENDING]")),
		);
	}

	await runtime.session.prompt(
		`Do this exactly in this turn:
1. Call read on packages/orchestrator/src/orchestrator.ts.
2. Call read on packages/app/src/runtime.ts.
3. Do not call bash or any other tool.
4. Answer in 4 short bullets explaining relay startup and restore wiring.`,
	);

	await waitFor(
		async () => existsSync(rootWorklogFile) && (await readFile(rootWorklogFile, "utf8")).trim().length > 0,
		120_000,
		250,
		"root worklog",
	);
	report.checks.rootWorklogCreated = true;

	const tickerCommand =
		"sh -lc 'for i in $(seq 1 8); do printf \"tick:%s\\n\" \"$i\"; sleep 1; done'";
	const tickerChildPrompt = `Do this exactly:
1. Do not spawn, message, read, or use any tool except bash and report.
2. Call bash exactly once with this exact command:
${tickerCommand}
3. After bash completes, call report exactly once with exactly this sentence and nothing else:
tick task finished after eight one-second intervals.
4. Stop.`;

	await runtime.session.prompt(`Delegate one small long-running task. Do this exactly:
1. Call spawn exactly once.
2. Spawn role "ticker" with this exact prompt:
${tickerChildPrompt}
3. Do not call bash, read, message, or any other tool yourself in this turn.
4. After the spawn call resolves, reply with exactly this sentence and nothing else:
Ticker delegated.`);
	report.checks.tickerDelegationReplySeen = runtime.session.getLastAssistantText()?.trim() === "Ticker delegated.";

	const tickerDispatch = await waitFor(() => {
		if (!existsSync(treeFile)) {
			return false;
		}
		const tree = readTree(treeFile);
		const tickerChild = Object.values(tree.agents).find(
			(entry) => entry.parentId === "root" && entry.role === "ticker" && entry.status === "running",
		);
		return tree.agents.root?.status === "idle" && tickerChild ? { tree, tickerChild } : false;
	}, 120_000, 100, "root idle while ticker child runs");
	report.tickerChildId = tickerDispatch.tickerChild.id;
	report.checks.rootIdleWhileTickerRunning = true;

	await runtime.session.prompt(
		"Reply with exactly this sentence and nothing else: Root still available.",
	);
	const followUpCompletedAt = Date.now();
	report.rootFollowUpCompletedAt = followUpCompletedAt;
	report.checks.rootAcceptedFollowUpDuringTicker =
		runtime.session.getLastAssistantText()?.trim() === "Root still available.";

	const tickerAfterFollowUp = readTree(treeFile).agents[report.tickerChildId];
	report.checks.tickerStillRunningAfterFollowUp = tickerAfterFollowUp?.status === "running";

	const tickerReport = await waitFor(() => {
		const message = customMessages("agent_report").find(
			(entry) =>
				entry.details?.fromRole === "ticker" &&
				extractTextContent(entry.content).includes("tick task finished after eight one-second intervals."),
		);
		return message || false;
	}, 240_000, 100, "ticker report");
	report.checks.tickerReportSeen = true;
	report.checks.tickerReportArrivedAfterFollowUp = timestampMs(tickerReport.timestamp) >= followUpCompletedAt;

	const tickerIdle = await waitFor(() => {
		const message = customMessages("agent_idle").find(
			(entry) => entry.details?.fromRole === "ticker" && entry.details?.fromAgentId === report.tickerChildId,
		);
		return message || false;
	}, 240_000, 100, "ticker idle notification");
	report.checks.tickerIdleSeen = true;
	report.checks.tickerIdleArrivedAfterFollowUp = timestampMs(tickerIdle.timestamp) >= followUpCompletedAt;
	await runtime.session.agent.waitForIdle();

	const worklogChildPrompt =
		"Inspect packages/orchestrator/src/worklog.ts, packages/orchestrator/src/context-transform.ts, packages/orchestrator/src/roster.ts, packages/orchestrator/src/messages.ts, packages/orchestrator/src/types.ts, the worklog-related sections of packages/orchestrator/src/orchestrator.ts, plus packages/orchestrator/test/worklog.test.ts and packages/orchestrator/test/roster.test.ts. Read those files, then send one short report summarizing worklog generation, ancestor propagation, roster injection, and how the tests cover that behavior. Do not call bash or message.";
	const startupChildPrompt =
		"Inspect packages/orchestrator/src/extension.ts, packages/orchestrator/src/session-factory.ts, packages/orchestrator/src/types.ts, packages/orchestrator/test/extension.test.ts, packages/orchestrator/test/session-restore.test.ts, packages/orchestrator/test/orchestrator.test.ts, packages/app/src/runtime.ts, and packages/app/test/runtime.test.ts. Read those files, then send one short report summarizing startup, session creation, restore wiring, and the test coverage for those paths. Do not call bash or message.";
	const backgroundCommand =
		"sh -lc 'sleep 8; printf \"packages: \"; find packages -name \"*.ts\" | wc -l; printf \"pi-mono/packages: \"; find pi-mono/packages -name \"*.ts\" | wc -l'";

	await runtime.session.prompt(`Exercise the relay runtime itself. Do this exactly:
1. Do not read any files yourself.
2. Emit one assistant response that contains exactly three tool calls before you wait for any tool result.
3. Call spawn twice and only twice.
4. First spawn: role "worklog-inspector" with this exact prompt:
${worklogChildPrompt}
5. Second spawn: role "startup-inspector" with this exact prompt:
${startupChildPrompt}
6. Launch one bash call in the background with __background=true using this exact command:
${backgroundCommand}
7. Do not call message, read, or any other tool in this turn.
8. After those three tool calls resolve, reply with exactly this sentence and nothing else:
Waiting for child and background updates.
9. After that sentence, stop. Do not message children, summarize their updates, or otherwise react unless I explicitly ask in a later turn.`);
	const dispatchCompletedAt = Date.now();
	report.dispatchCompletedAt = dispatchCompletedAt;

	const dispatchTree = await waitFor(() => {
		if (!existsSync(treeFile)) {
			return false;
		}
		const tree = readTree(treeFile);
		const children = Object.values(tree.agents).filter(
			(entry) =>
				entry.parentId === "root" &&
				(entry.role === "worklog-inspector" || entry.role === "startup-inspector") &&
				entry.status !== "disposed",
		);
		return children.length === 2 ? tree : false;
	}, 120_000, 100, "two spawned children");
	report.dispatchSnapshot = dispatchTree;
	const dispatchChildren = Object.values(dispatchTree.agents).filter(
		(entry) =>
			entry.parentId === "root" &&
			(entry.role === "worklog-inspector" || entry.role === "startup-inspector") &&
			entry.status !== "disposed",
	);
	report.checks.rootReturnedDispatchReply = runtime.session.getLastAssistantText()?.trim() === "Waiting for child and background updates.";

	await waitFor(() => pendingBashResults().length > 0, 120_000, 100, "pending background bash");
	report.checks.backgroundPendingSeen = true;

	const completionMessages = await waitFor(() => {
		const messages = customMessages("bg_tool_completion");
		return messages.length > 0 ? messages : false;
	}, 240_000, 100, "background completion");

	const completion = completionMessages.at(-1);
	report.checks.backgroundCompletionSeen = true;
	report.backgroundCompletion = {
		content: completion.content,
		details: completion.details,
	};

	const completionText = extractTextContent(completion.content);
	const outputPathMatch = completionText.match(/Combined stdout\/stderr: (.+)$/m);
	report.checks.backgroundOutputPathAdvertised = Boolean(outputPathMatch);
	if (outputPathMatch?.[1]) {
		report.backgroundOutputPath = outputPathMatch[1];
		report.checks.backgroundOutputPathExists = existsSync(outputPathMatch[1]);
	}

	const reportMessages = await waitFor(() => {
		const messages = customMessages("agent_report").filter(
			(entry) => entry.details?.fromRole === "worklog-inspector" || entry.details?.fromRole === "startup-inspector",
		);
		return messages.length >= 2 ? messages : false;
	}, 240_000, 100, "two child reports");
	report.checks.twoChildReportsSeen = reportMessages.length >= 2;

	const idleMessages = await waitFor(() => {
		const messages = customMessages("agent_idle").filter(
			(entry) => entry.details?.fromRole === "worklog-inspector" || entry.details?.fromRole === "startup-inspector",
		);
		return messages.length >= 2 ? messages : false;
	}, 240_000, 100, "two child idle notifications");
	report.checks.twoChildIdleNotificationsSeen = idleMessages.length >= 2;

	const childrenSettledTree = await waitFor(() => {
		const tree = readTree(treeFile);
		const children = Object.values(tree.agents).filter(
			(entry) =>
				entry.parentId === "root" &&
				(entry.role === "worklog-inspector" || entry.role === "startup-inspector") &&
				entry.status !== "disposed",
		);
		if (children.length === 2 && children.every((entry) => entry.status === "idle")) {
			return tree;
		}
		return false;
	}, 240_000, 100, "children persisted as idle");
	report.checks.childrenPersistedIdle = true;

	const childEntries = Object.values(childrenSettledTree.agents).filter(
		(entry) =>
			entry.parentId === "root" &&
			(entry.role === "worklog-inspector" || entry.role === "startup-inspector") &&
			entry.status !== "disposed",
	);
	report.childStatuses = Object.fromEntries(childEntries.map((entry) => [entry.id, entry.status]));

	const childContexts = [];
	const childWorklogs = [];
	for (const child of childEntries) {
		const childSession = SessionManager.open(child.sessionFile, sessionDir, repoDir);
		const context = childSession.buildSessionContext().messages;
		const firstUser = context.find((message) => message.role === "user");
		const firstUserText = extractTextContent(firstUser?.content);
		const worklogText = existsSync(child.worklogFile) ? await readFile(child.worklogFile, "utf8") : "";
		childContexts.push({
			id: child.id,
			hasAncestorWorklog: firstUserText.includes('<ancestor-worklog agent="root" role="root">'),
			firstUserText,
		});
		childWorklogs.push({
			id: child.id,
			file: child.worklogFile,
			hasContent: worklogText.trim().length > 0,
			entryCount: (worklogText.match(/## Entry —/g) ?? []).length,
		});
	}

	report.childContexts = childContexts;
	report.childWorklogs = childWorklogs;
	report.checks.childrenInheritedAncestorWorklog = childContexts.every((entry) => entry.hasAncestorWorklog);
	report.checks.someChildWorklogFilesPopulated = childWorklogs.some((entry) => entry.hasContent);
	report.checks.rootDidNotReceiveChildWorklogs =
		customMessages("agent_worklog").filter(
			(entry) => entry.details?.fromRole === "worklog-inspector" || entry.details?.fromRole === "startup-inspector",
		).length === 0;

	await runtime.session.agent.waitForIdle();
	report.settledSnapshot = readTree(treeFile);
	const summaryMessageStart = runtime.session.agent.state.messages.length;
	await runtime.session.prompt("Summarize in 5 short bullets what the child agents reported and what the background bash returned.");
	const summaryMessages = runtime.session.agent.state.messages.slice(summaryMessageStart).filter((message) => message.role === "assistant");
	const finalSummaryMessage = [...summaryMessages]
		.reverse()
		.find((message) => extractTextContent(message.content).trim().length > 0);
	report.finalAssistantText = finalSummaryMessage ? extractTextContent(finalSummaryMessage.content) : "";
	report.checks.finalSummaryGenerated = Boolean(report.finalAssistantText);

	report.success = Object.values(report.checks).every(Boolean);
	saveReport();
	console.log(JSON.stringify(report, null, 2));
	process.exit(report.success ? 0 : 1);
} catch (error) {
	report.error = error instanceof Error ? { message: error.message, stack: error.stack } : String(error);
	saveReport();
	console.error(error);
	process.exit(1);
}
