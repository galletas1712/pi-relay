/**
 * pi-ai Sidecar Bridge — HTTP server wrapping @oh-my-pi/pi-ai.
 *
 * Exposes JSON-RPC-like methods that the Rust PiAiSidecarProvider calls:
 *   complete          — model turn (pi-ai complete())
 *   compact           — context compaction (native or prompt-based)
 *   models.available  — list available models with metadata
 *   web_search        — provider-native or neutral web search
 *
 * Design docs: .pi/pi-ai-sidecar/DESIGN.md, CLEAN-SEAM.md, SCOPE.md
 */

import http from "node:http";
import { readFile } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

// ── Configuration ───────────────────────────────────────────────────────────

const PORT = parseInt(process.env.SIDECAR_PORT || process.env.PORT || "8732", 10);
const HOST = process.env.SIDECAR_HOST || "127.0.0.1";
const AGENT_DIR = process.env.SIDECAR_AGENT_DIR || process.env.AGENT_DIR || "";

// ── Lazy pi-ai loader ───────────────────────────────────────────────────────
//
// @oh-my-pi/pi-ai ships TypeScript source and requires Bun.  We import it
// lazily so the HTTP server starts and serves /healthz even when the package
// is not installed or the runtime cannot load .ts files.  Full method
// functionality is available once the package resolves (e.g. under `bun`).

let _piAi = null;
let _piAiLoadError = null;

async function loadPiAi() {
  if (_piAi) return _piAi;
  if (_piAiLoadError) throw _piAiLoadError;
  try {
    _piAi = await import("@oh-my-pi/pi-ai");
    return _piAi;
  } catch (err) {
    _piAiLoadError = err;
    throw err;
  }
}

let _piCatalog = null;
let _piCatalogLoadError = null;

async function loadPiCatalog() {
  if (_piCatalog) return _piCatalog;
  if (_piCatalogLoadError) throw _piCatalogLoadError;
  try {
    _piCatalog = await import("@oh-my-pi/pi-catalog");
    return _piCatalog;
  } catch (err) {
    _piCatalogLoadError = err;
    throw err;
  }
}

// ── Auth storage (lazy) ─────────────────────────────────────────────────────

let _authStorage = null;

async function getAuthStorage() {
  if (_authStorage) return _authStorage;
  const piAi = await loadPiAi();
  if (!piAi.AuthStorage) return null;
  const dbPath = AGENT_DIR ? join(AGENT_DIR, "auth.db") : undefined;
  _authStorage = new piAi.AuthStorage({ dbPath });
  return _authStorage;
}

// ── models.json loader ──────────────────────────────────────────────────────

async function loadModelsJson() {
  if (!AGENT_DIR) return { providers: {} };
  const path = join(AGENT_DIR, "models.json");
  try {
    const content = await readFile(path, "utf-8");
    return JSON.parse(content);
  } catch {
    return { providers: {} };
  }
}

// ── Model resolution ────────────────────────────────────────────────────────

/**
 * Resolve a (provider, modelId) pair to a pi-ai Model object.
 *
 * Strategy:
 * 1. Check models.json for a custom provider/model definition.
 * 2. Fall back to the bundled catalog from @oh-my-pi/pi-catalog.
 */
async function resolveModel(provider, modelId) {
  const modelsJson = await loadModelsJson();
  const providerConfig = modelsJson.providers?.[provider];

  // Try custom model from models.json
  if (providerConfig?.models) {
    const customModel = providerConfig.models.find(
      (m) => m.id === modelId || m.name === modelId,
    );
    if (customModel) {
      return buildModelFromConfig(provider, customModel, providerConfig);
    }
  }

  // Fall back to bundled catalog
  const catalog = await loadPiCatalog();
  if (catalog.getBundledModel) {
    const model = catalog.getBundledModel(provider, modelId);
    if (model) return model;
  }

  // Last resort: try getBundledModels and search
  if (catalog.getBundledModels) {
    const models = catalog.getBundledModels(provider);
    const model = models?.find(
      (m) => m.id === modelId || m.name === modelId,
    );
    if (model) return model;
  }

  throw new Error(
    `Model not found: provider="${provider}" model="${modelId}"`,
  );
}

/**
 * Build a pi-ai-compatible Model object from a models.json entry.
 */
function buildModelFromConfig(providerId, modelDef, providerConfig) {
  const api = modelDef.api || providerConfig.api || "openai-completions";
  const baseUrl = modelDef.baseUrl || providerConfig.baseUrl || "";
  return {
    id: modelDef.id,
    name: modelDef.name || modelDef.id,
    api,
    provider: providerId,
    baseUrl,
    reasoning: modelDef.reasoning ?? false,
    input: modelDef.input || ["text"],
    cost: modelDef.cost || {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
    },
    contextWindow: modelDef.contextWindow || 200000,
    maxTokens: modelDef.maxTokens || 64000,
    headers: { ...providerConfig.headers, ...modelDef.headers },
    compat: modelDef.compat || providerConfig.compat,
  };
}

// ── Transcript mapping (Rust → pi-ai) ───────────────────────────────────────

/**
 * Convert Rust ModelTranscriptEntry[] to pi-ai Message[].
 *
 * Rust transcript entries are tagged objects: { item: { type: "...", ...fields }, provider_replay: [] }
 * The `item` field carries a TranscriptItem variant.
 *
 * Filtering: skip TurnStarted, ToolCallStarted, TurnFinished, DaemonToolObservation.
 * Mapping:
 *   UserMessage       → { role: "user", content, timestamp }
 *   AssistantMessage  → { role: "assistant", content, timestamp }
 *   ToolResult        → { role: "toolResult", toolCallId, toolName, content, isError, timestamp }
 *   CompactionSummary → { role: "user", content: [{ type: "text", text: summary }], timestamp }
 */
function mapTranscript(transcript) {
  if (!Array.isArray(transcript)) return [];
  const messages = [];
  for (const entry of transcript) {
    // Entries may be { item: { type, ... } } or bare { type, ... }
    const item = entry.item || entry;
    const type = item.type;
    switch (type) {
      case "turn_started":
      case "tool_call_started":
      case "turn_finished":
      case "daemon_tool_observation":
        // Skip — these are control/daemon entries, not model-visible messages
        break;

      case "user_message": {
        messages.push(mapUserMessage(item));
        break;
      }

      case "assistant_message": {
        messages.push(mapAssistantMessage(item));
        break;
      }

      case "tool_result": {
        messages.push(mapToolResult(item));
        break;
      }

      case "compaction_summary": {
        // Render as a user message with the summary text
        messages.push({
          role: "user",
          content: [
            { type: "text", text: item.summary || "" },
          ],
          timestamp: Date.now(),
        });
        break;
      }

      default:
        // Unknown entry type — skip rather than risk a malformed message
        break;
    }
  }
  return messages;
}

/**
 * Map Rust UserMessage to pi-ai UserMessage.
 *
 * Rust: { content: [{ type: "text", text }, { type: "image", image: { mime_type, source: { kind: "base64", value } } }] }
 * pi-ai: { role: "user", content: string | (TextContent | ImageContent)[], timestamp }
 */
function mapUserMessage(item) {
  // Rust UserMessage wraps content in a `content` array of ContentBlock
  const content = item.content || item.items || [];
  if (typeof content === "string") {
    return { role: "user", content, timestamp: Date.now() };
  }
  const blocks = content.map(mapContentBlock).filter(Boolean);
  // If all blocks are text, collapse to a simple string for efficiency
  if (blocks.length > 0 && blocks.every((b) => b.type === "text")) {
    return {
      role: "user",
      content: blocks.map((b) => b.text).join(""),
      timestamp: Date.now(),
    };
  }
  return { role: "user", content: blocks, timestamp: Date.now() };
}

/**
 * Map Rust AssistantMessage to pi-ai AssistantMessage.
 *
 * Rust: { items: [{ type: "text", text }, { type: "tool_call", id, tool_name, args_json }] }
 * pi-ai: { role: "assistant", content: [TextContent | ThinkingContent | ToolCall], timestamp }
 */
function mapAssistantMessage(item) {
  const items = item.items || [];
  const content = [];
  for (const assistantItem of items) {
    switch (assistantItem.type) {
      case "text":
        content.push({ type: "text", text: assistantItem.text });
        break;
      case "thinking":
        content.push({
          type: "thinking",
          thinking: assistantItem.thinking || "",
          ...(assistantItem.signature
            ? { thinkingSignature: assistantItem.signature }
            : {}),
        });
        break;
      case "tool_call":
        content.push({
          type: "toolCall",
          id: assistantItem.id,
          name: assistantItem.tool_name || assistantItem.name,
          arguments: parseJsonArgs(assistantItem.args_json),
        });
        break;
      default:
        break;
    }
  }
  return { role: "assistant", content, timestamp: Date.now() };
}

/**
 * Map Rust ToolResultMessage to pi-ai ToolResultMessage.
 *
 * Rust: { tool_call_id, tool_name, output, status: "success"|"error"|"interrupted"|"crashed" }
 * pi-ai: { role: "toolResult", toolCallId, toolName, content: [TextContent], isError, timestamp }
 */
function mapToolResult(item) {
  const isError =
    item.status === "error" ||
    item.status === "interrupted" ||
    item.status === "crashed";
  return {
    role: "toolResult",
    toolCallId: item.tool_call_id,
    toolName: item.tool_name,
    content: [{ type: "text", text: item.output || "" }],
    isError,
    timestamp: Date.now(),
  };
}

/**
 * Map a Rust ContentBlock to a pi-ai content block.
 *
 * Rust: { type: "text", text } | { type: "image", image: { mime_type, source: { kind: "base64"|"url", value } } }
 * pi-ai: { type: "text", text } | { type: "image", data, mimeType }
 */
function mapContentBlock(block) {
  if (!block) return null;
  switch (block.type) {
    case "text":
      return { type: "text", text: block.text };
    case "image": {
      const img = block.image;
      const source = img?.source;
      if (source?.kind === "base64" || source?.value) {
        return {
          type: "image",
          data: source.value,
          mimeType: img.mime_type || img.mimeType || "image/png",
        };
      }
      if (source?.kind === "url" || source?.value) {
        // pi-ai expects base64 data; for URLs we pass through as-is
        return {
          type: "image",
          data: source.value,
          mimeType: img.mime_type || img.mimeType || "image/png",
        };
      }
      return null;
    }
    default:
      return null;
  }
}

/**
 * Parse a JSON args string to an object. Returns {} on failure.
 */
function parseJsonArgs(argsJson) {
  if (!argsJson) return {};
  if (typeof argsJson === "object") return argsJson;
  try {
    return JSON.parse(argsJson);
  } catch {
    return {};
  }
}

// ── Tools mapping (Rust → pi-ai) ────────────────────────────────────────────

/**
 * Convert Rust ProviderTool[] to pi-ai Tool[].
 *
 * Rust ProviderTool: { canonical_name, name, description, input_schema, declaration, execution }
 * pi-ai Tool: { name, description, parameters }
 *
 * Per CLEAN-SEAM design: pass canonical tool defs only (name, description,
 * parameters). pi-ai builds all provider-specific wire format (tool
 * declarations, cache markers, etc.) internally.
 */
function mapTools(tools) {
  if (!Array.isArray(tools)) return [];
  return tools
    .map((tool) => ({
      name: tool.name || tool.canonical_name,
      description: tool.description || "",
      parameters: tool.input_schema || tool.parameters || {},
    }))
    .filter((t) => t.name);
}

// ── Prompt mapping ──────────────────────────────────────────────────────────

/**
 * Convert Rust PromptSections to pi-ai Context.systemPrompt (string[]).
 *
 * Rust: { stable_prefix, dynamic_context }
 * pi-ai: string[] (array of system prompt blocks)
 */
function mapSystemPrompt(prompt) {
  if (!prompt) return undefined;
  const blocks = [];
  if (prompt.stable_prefix) blocks.push(prompt.stable_prefix);
  if (prompt.dynamic_context) blocks.push(prompt.dynamic_context);
  return blocks.length > 0 ? blocks : undefined;
}

// ── Reasoning effort mapping ─────────────────────────────────────────────────

/**
 * Map Rust ReasoningEffort string to pi-ai Effort string.
 *
 * Rust: "none"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max"
 * pi-ai: "minimal"|"low"|"medium"|"high"|"xhigh"|"max" (no "none" — omit to disable)
 */
function mapReasoningEffort(effort) {
  if (!effort || effort === "none") return undefined;
  const valid = ["minimal", "low", "medium", "high", "xhigh", "max"];
  return valid.includes(effort) ? effort : undefined;
}

// ── Cache retention mapping ─────────────────────────────────────────────────

/**
 * Map Rust CacheRetention to pi-ai CacheRetention.
 *
 * Rust: "none"|"short"|"long"
 * pi-ai: "none"|"short"|"long"
 */
function mapCacheRetention(retention) {
  if (!retention) return undefined;
  const valid = ["none", "short", "long"];
  return valid.includes(retention) ? retention : undefined;
}

// ── Method handlers ─────────────────────────────────────────────────────────

/**
 * complete — model turn via pi-ai complete().
 *
 * Request: ModelRequest JSON (Rust wire format)
 * Response: pi-ai AssistantMessage JSON
 */
async function handleComplete(req) {
  const piAi = await loadPiAi();

  const provider = req.provider || req.kind || "openai";
  const modelId = req.model;
  if (!modelId) throw new Error("Missing required field: model");

  const model = await resolveModel(provider, modelId);

  // Build pi-ai Context
  const systemPrompt = mapSystemPrompt(req.prompt);
  const messages = mapTranscript(req.transcript);
  const tools = mapTools(req.tools);

  const context = { messages };
  if (systemPrompt) context.systemPrompt = systemPrompt;
  if (tools.length > 0) context.tools = tools;

  // Build stream options (SimpleStreamOptions — supports reasoning + apiKey)
  const options = {};
  if (req.max_tokens != null) options.maxTokens = req.max_tokens;
  if (req.session_id) options.sessionId = req.session_id;
  if (req.prompt_cache_key) options.promptCacheKey = req.prompt_cache_key;

  const cacheRetention = mapCacheRetention(req.cache_retention);
  if (cacheRetention) options.cacheRetention = cacheRetention;

  // Map reasoning effort (Rust ReasoningEffort → pi-ai Effort)
  const reasoning = mapReasoningEffort(req.reasoning_effort);
  if (reasoning) options.reasoning = reasoning;

  // Resolve API key from auth storage, env, or models.json
  const apiKey = await resolveApiKey(provider, model);
  if (apiKey) options.apiKey = apiKey;

  // Use completeSimple() — it handles API key resolution, reasoning effort
  // mapping, and provider-specific option transformation (thinking budgets,
  // tool choice, cache retention, session affinity headers, etc.).
  return await piAi.completeSimple(model, context, options);
}

/**
 * compact — context compaction (native or prompt-based).
 *
 * Request: ProviderCompactionRequest JSON (Rust wire format)
 * Response: { summary: string, usage?: object }
 *
 * Decision logic:
 *   openai-codex  → native POST /responses/compact (V1) or V2 streaming
 *   anthropic      → native POST /messages with compaction beta header
 *   others        → completeSimple() with summarization prompt (fallback)
 */
async function handleCompact(req) {
  const piAi = await loadPiAi();

  const provider = req.provider || req.kind || "openai";
  const modelId = req.model;
  if (!modelId) throw new Error("Missing required field: model");

  const model = await resolveModel(provider, modelId);

  // Build messages from transcript for summarization
  const messages = mapTranscript(req.transcript);

  // Check if provider supports native compaction
  const compactionMode = await getCompactionMode(provider);

  if (compactionMode === "native") {
    // Try native compaction first
    try {
      const result = await nativeCompaction(
        piAi,
        provider,
        model,
        messages,
        req,
      );
      if (result) return result;
    } catch (err) {
      // Fall through to prompt-based compaction
      console.error("Native compaction failed, falling back to prompt-based");
    }
  }

  // Prompt-based compaction (works with any provider)
  return await promptBasedCompaction(piAi, model, messages, req);
}

/**
 * Determine compaction mode for a provider.
 * Checks models.json `compaction` field, then falls back to provider defaults.
 */
async function getCompactionMode(provider) {
  const modelsJson = await loadModelsJson();
  const providerConfig = modelsJson.providers?.[provider];
  if (providerConfig?.compaction) {
    return providerConfig.compaction; // "native" | "prompt"
  }
  // Default: native for known providers, prompt for others
  if (provider === "openai-codex" || provider === "anthropic") {
    return "native";
  }
  return "prompt";
}

/**
 * Native compaction for OpenAI Codex and Anthropic.
 * Uses provider-specific compaction endpoints.
 */
async function nativeCompaction(piAi, provider, model, messages, req) {
  if (provider === "openai-codex") {
    // OpenAI Codex: POST /responses/compact (V1) or V2 streaming compaction_trigger
    // The @oh-my-pi/pi-ai fork has built-in compaction utilities.
    // Try to use them; fall back to prompt-based if unavailable.
    if (piAi.shouldUseOpenAiRemoteCompaction) {
      const shouldUse = piAi.shouldUseOpenAiRemoteCompaction(model);
      if (shouldUse) {
        return await openAiRemoteCompaction(piAi, model, messages, req);
      }
    }
    return null; // Fall back to prompt-based
  }

  if (provider === "anthropic") {
    // Anthropic: POST /messages with compaction beta header
    // The @oh-my-pi/pi-ai fork may expose this; otherwise fall back.
    if (piAi.shouldUseProviderNativeCompaction) {
      const shouldUse = piAi.shouldUseProviderNativeCompaction(model);
      if (shouldUse) {
        return await anthropicNativeCompaction(piAi, model, messages, req);
      }
    }
    return null; // Fall back to prompt-based
  }

  return null;
}

/**
 * OpenAI remote compaction via POST /responses/compact.
 */
async function openAiRemoteCompaction(piAi, model, messages, req) {
  // The fork may expose a dedicated compaction function
  if (piAi.compactOpenAi) {
    return await piAi.compactOpenAi(model, messages, {
      instructions: req.compaction_instructions,
    });
  }
  return null;
}

/**
 * Anthropic native compaction via POST /messages with compaction beta.
 */
async function anthropicNativeCompaction(piAi, model, messages, req) {
  if (piAi.compactAnthropic) {
    return await piAi.compactAnthropic(model, messages, {
      instructions: req.compaction_instructions,
    });
  }
  return null;
}

// ── Summarization prompt (pi-coding-agent pattern) ──────────────────────────

const SUMMARIZATION_SYSTEM_PROMPT = `You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.`;

const SUMMARIZATION_PROMPT = `The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.`;

/**
 * Prompt-based compaction using completeSimple() with a summarization prompt.
 * This is the generic, model-agnostic fallback that works with any provider.
 */
async function promptBasedCompaction(piAi, model, messages, req) {
  // Serialize conversation to text
  const conversationText = serializeConversation(messages);
  const promptText = `<conversation>\n${conversationText}\n</conversation>\n\n${SUMMARIZATION_PROMPT}`;

  const summarizationMessages = [
    {
      role: "user",
      content: [{ type: "text", text: promptText }],
      timestamp: Date.now(),
    },
  ];

  const reserveTokens = 16384;
  const maxTokens = Math.min(
    Math.floor(0.8 * reserveTokens),
    model.maxTokens > 0 ? model.maxTokens : 64000,
  );

  const options = {
    maxTokens,
    cacheRetention: "none",
    sessionId: undefined, // Isolate routing for compaction
  };

  const reasoning = mapReasoningEffort(req.reasoning_effort);
  if (reasoning && model.reasoning) {
    options.reasoning = reasoning;
  }

  const apiKey = await resolveApiKey(model.provider, model);
  if (apiKey) options.apiKey = apiKey;

  const response = await piAi.completeSimple(
    model,
    {
      systemPrompt: [SUMMARIZATION_SYSTEM_PROMPT],
      messages: summarizationMessages,
    },
    options,
  );

  if (response.stopReason === "error") {
    throw new Error(
      `Summarization failed: ${response.errorMessage || "Unknown error"}`,
    );
  }

  // Extract text from the response
  const summary = extractText(response.content);

  return {
    summary,
    usage: response.usage,
  };
}

/**
 * Serialize pi-ai messages to a text representation for the summarization prompt.
 */
function serializeConversation(messages) {
  const lines = [];
  for (const msg of messages) {
    switch (msg.role) {
      case "user": {
        const text = typeof msg.content === "string"
          ? msg.content
          : msg.content.map((b) => b.text || "").join("");
        lines.push(`[User]: ${text}`);
        break;
      }
      case "assistant": {
        const parts = [];
        for (const block of msg.content) {
          if (block.type === "text") parts.push(block.text);
          else if (block.type === "thinking") parts.push(`(thinking: ${block.thinking})`);
          else if (block.type === "toolCall")
            parts.push(`(tool call: ${block.name}(${JSON.stringify(block.arguments)}))`);
        }
        lines.push(`[Assistant]: ${parts.join(" ")}`);
        break;
      }
      case "toolResult": {
        const text = msg.content.map((b) => b.text || "").join("");
        lines.push(`[Tool Result (${msg.toolName})]: ${text}`);
        break;
      }
      default:
        break;
    }
  }
  return lines.join("\n\n");
}

/**
 * Extract text content from a pi-ai AssistantMessage content array.
 */
function extractText(content) {
  if (!Array.isArray(content)) return "";
  return content
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("");
}

// ── models.available ────────────────────────────────────────────────────────

/**
 * models.available — return available models with metadata.
 *
 * Response: { models: [{ id, name, provider, api, contextWindow, maxTokens, reasoning, ... }] }
 */
async function handleModelsAvailable(req) {
  const modelsJson = await loadModelsJson();

  const allModels = [];

  // 1. Custom models from models.json (available without pi-catalog)
  if (modelsJson.providers) {
    for (const [providerId, providerConfig] of Object.entries(
      modelsJson.providers,
    )) {
      if (providerConfig.models) {
        for (const modelDef of providerConfig.models) {
          allModels.push({
            id: modelDef.id,
            name: modelDef.name || modelDef.id,
            provider: providerId,
            api: modelDef.api || providerConfig.api || "openai-completions",
            baseUrl: modelDef.baseUrl || providerConfig.baseUrl || "",
            reasoning: modelDef.reasoning ?? false,
            input: modelDef.input || ["text"],
            contextWindow: modelDef.contextWindow || 200000,
            maxTokens: modelDef.maxTokens || 64000,
            cost: modelDef.cost,
          });
        }
      }
    }
  }

  // 2. Bundled catalog models (requires @oh-my-pi/pi-catalog)
  try {
    const catalog = await loadPiCatalog();
    if (catalog.getBundledProviders && catalog.getBundledModels) {
      const providers = catalog.getBundledProviders();
      for (const provider of providers) {
        try {
          const models = catalog.getBundledModels(provider);
          for (const model of models) {
            // Avoid duplicates from models.json
            const exists = allModels.some(
              (m) => m.provider === provider && m.id === model.id,
            );
            if (!exists) {
              allModels.push({
                id: model.id,
                name: model.name,
                provider: model.provider,
                api: model.api,
                baseUrl: model.baseUrl,
                reasoning: model.reasoning,
                input: model.input,
                contextWindow: model.contextWindow,
                maxTokens: model.maxTokens,
                cost: model.cost,
                ...(model.capabilities
                  ? { capabilities: model.capabilities }
                  : {}),
              });
            }
          }
        } catch {
          // Skip providers that fail to load
        }
      }
    }
  } catch {
    // pi-catalog not available — return only models.json models
  }

  return { models: allModels };
}

// ── web_search ──────────────────────────────────────────────────────────────

/**
 * web_search — provider-native or neutral web search.
 *
 * Request: { provider, model, query, ... }
 * Response: { answer, sources, citations }
 *
 * For anthropic: use Anthropic's web_search_20250305 tool via complete()
 * For others: HTTP web search fallback
 */
async function handleWebSearch(req) {
  const provider = req.provider || req.kind || "openai";
  const query = req.query;
  if (!query) throw new Error("Missing required field: query");

  const webSearchMode = await getWebSearchMode(provider);

  if (webSearchMode === "native") {
    return await nativeWebSearch(req, provider);
  }

  // Neutral HTTP web search fallback
  return await neutralWebSearch(query);
}

/**
 * Determine web search mode for a provider.
 */
async function getWebSearchMode(provider) {
  const modelsJson = await loadModelsJson();
  const providerConfig = modelsJson.providers?.[provider];
  if (providerConfig?.webSearch) {
    return providerConfig.webSearch; // "native" | "neutral" | "disabled"
  }
  // Default: native for known providers, neutral for others
  if (provider === "anthropic" || provider === "openai" || provider === "openai-codex") {
    return "native";
  }
  return "neutral";
}

/**
 * Provider-native web search.
 * For Anthropic: use the web_search_20250305 server tool via complete().
 */
async function nativeWebSearch(req, provider) {
  const piAi = await loadPiAi();
  const modelId = req.model;
  if (!modelId) throw new Error("Missing required field: model");

  const model = await resolveModel(provider, modelId);
  const query = req.query;

  // Build a context with the search query as a user message
  const context = {
    messages: [
      {
        role: "user",
        content: query,
        timestamp: Date.now(),
      },
    ],
  };

  // For Anthropic: declare the web_search tool
  if (provider === "anthropic") {
    context.tools = [
      {
        name: "web_search",
        description:
          "Search the web for current information. Returns search results with sources.",
        parameters: {
          type: "object",
          properties: {
            query: {
              type: "string",
              description: "The search query",
            },
          },
          required: ["query"],
        },
      },
    ];
  }

  const options = {};
  const apiKey = await resolveApiKey(provider, model);
  if (apiKey) options.apiKey = apiKey;

  const response = await piAi.completeSimple(model, context, options);

  // Extract answer and sources from the response
  const answer = extractText(response.content);
  const sources = [];
  const citations = [];

  // Extract server tool results and citations from content
  for (const block of response.content) {
    if (block.type === "toolCall" && block.name === "web_search") {
      sources.push(block.arguments);
    }
    // Some providers return server tool results with citation data
    if (block.type === "server_tool_result" || block.type === "serverToolResult") {
      if (block.output) {
        for (const result of block.output) {
          if (result.url) sources.push({ url: result.url, title: result.title });
          if (result.citation) citations.push(result.citation);
        }
      }
    }
  }

  return { answer, sources, citations };
}

/**
 * Neutral HTTP web search fallback.
 * Uses a simple HTTP-based search (no provider API call).
 */
async function neutralWebSearch(query) {
  // Minimal implementation: return an empty result with a note.
  // The daemon's web_tools.rs can provide its own HTTP search implementation.
  return {
    answer: "",
    sources: [],
    citations: [],
    note: "Neutral web search fallback — configure a search provider or use provider-native web search.",
  };
}

// ── API key resolution ─────────────────────────────────────────────────────

/**
 * Resolve the API key for a provider.
 *
 * Priority:
 * 1. AuthStorage (from auth.json/auth.db)
 * 2. Environment variable (via pi-ai's getEnvApiKey)
 * 3. models.json provider config apiKey
 */
async function resolveApiKey(provider, model) {
  // Try AuthStorage first
  try {
    const authStorage = await getAuthStorage();
    if (authStorage) {
      const apiKey = await authStorage.getApiKey(provider);
      if (apiKey) return apiKey;
    }
  } catch {
    // AuthStorage not available — fall through
  }

  // Try pi-ai's getEnvApiKey
  try {
    const piAi = await loadPiAi();
    if (piAi.getEnvApiKey) {
      const apiKey = piAi.getEnvApiKey(provider);
      if (apiKey) return apiKey;
    }
  } catch {
    // pi-ai not available — fall through
  }

  // Try models.json provider config
  const modelsJson = await loadModelsJson();
  const providerConfig = modelsJson.providers?.[provider];
  if (providerConfig?.apiKey) return providerConfig.apiKey;

  return undefined;
}

// ── HTTP server ─────────────────────────────────────────────────────────────

/**
 * Read the full request body as JSON.
 */
function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf-8");
      if (!raw) return resolve({});
      try {
        resolve(JSON.parse(raw));
      } catch {
        reject(new Error("Invalid JSON in request body"));
      }
    });
    req.on("error", reject);
  });
}

/**
 * Send a JSON response.
 */
function sendJson(res, status, body) {
  const json = JSON.stringify(body);
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(json),
  });
  res.end(json);
}

/**
 * Log an error without leaking secrets.
 * Strips API keys, bearer tokens, and other sensitive data.
 */
function safeLogError(message, err) {
  const msg = err?.message || String(err);
  // Redact common secret patterns
  const redacted = msg
    .replace(/sk-[a-zA-Z0-9_-]+/g, "sk-[REDACTED]")
    .replace(/Bearer\s+[a-zA-Z0-9_.-]+/gi, "Bearer [REDACTED]")
    .replace(/api[_-]?key["\s:=]+[^\s"']+/gi, "api_key=[REDACTED]")
    .replace(/token["\s:=]+[^\s"']+/gi, "token=[REDACTED]");
  console.error(`[sidecar] ${message}: ${redacted}`);
}

/**
 * Main HTTP request handler.
 */
async function handleRequest(req, res) {
  // Health check
  if (req.method === "GET" && req.url === "/healthz") {
    return sendJson(res, 200, { ok: true });
  }

  // All other methods go through POST /
  if (req.method !== "POST" || (req.url !== "/" && req.url !== "")) {
    return sendJson(res, 404, { error: "Not found" });
  }

  let body;
  try {
    body = await readBody(req);
  } catch (err) {
    return sendJson(res, 400, { error: "Invalid JSON body" });
  }

  const method = body.method;
  if (!method) {
    return sendJson(res, 400, { error: "Missing required field: method" });
  }

  try {
    let result;
    switch (method) {
      case "complete":
        result = await handleComplete(body);
        break;
      case "compact":
        result = await handleCompact(body);
        break;
      case "models.available":
        result = await handleModelsAvailable(body);
        break;
      case "web_search":
        result = await handleWebSearch(body);
        break;
      default:
        return sendJson(res, 400, { error: `Unknown method: ${method}` });
    }
    return sendJson(res, 200, result);
  } catch (err) {
    safeLogError(`method "${method}" failed`, err);
    const status = err.message?.includes("not found") ? 404 : 500;
    return sendJson(res, status, {
      error: err.message || "Internal server error",
    });
  }
}

// ── Start server ────────────────────────────────────────────────────────────

const server = http.createServer((req, res) => {
  handleRequest(req, res).catch((err) => {
    safeLogError("unhandled error", err);
    if (!res.headersSent) {
      sendJson(res, 500, { error: "Internal server error" });
    }
  });
});

server.listen(PORT, HOST, () => {
  // Log only non-sensitive info — no secrets, no API keys
  console.log(`[sidecar] listening on http://${HOST}:${PORT}`);
  if (AGENT_DIR) {
    console.log(`[sidecar] agent dir: ${AGENT_DIR}`);
  } else {
    console.log("[sidecar] agent dir: (not set — set SIDECAR_AGENT_DIR or AGENT_DIR)");
  }
});

// Graceful shutdown
process.on("SIGTERM", () => {
  console.log("[sidecar] shutting down");
  server.close(() => process.exit(0));
});

process.on("SIGINT", () => {
  console.log("[sidecar] interrupt received");
  server.close(() => process.exit(0));
});

export { handleComplete, handleCompact, handleModelsAvailable, handleWebSearch };
