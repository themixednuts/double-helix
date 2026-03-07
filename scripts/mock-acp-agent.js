#!/usr/bin/env node
/**
 * Mock ACP agent for testing Helix ACP integration.
 * Uses ACP schema: agentclientprotocol.com/protocol/session-config-options
 *
 * Implements: initialize, session/new, session/prompt, session/set_config_option, session/cancel.
 * Mode (S-tab) = configOptions category "mode". Model (C-m) = category "model".
 * Selected mode value runs on prompt. Type mode name as first word to override.
 *
 * Usage: node scripts/mock-acp-agent.js
 */

const readline = require("readline");

const CANCELLED_SESSIONS = new Set();

// Mode config option values (ACP category "mode") — must match options[].value exactly
const MODE_VALUES = [
  "echo",
  "long",
  "big",
  "slow",
  "error",
  "refusal",
  "max_tokens",
  "plan",
  "tool",
  "config",
  "mode",
  "help",
];

// Model config option values (ACP category "model")
const MODEL_VALUES = ["a", "b", "c"];

const LOREM =
  "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor " +
  "incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis " +
  "nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.\n\n";

const LARGE_BODY =
  LOREM +
  "Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore " +
  "eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt " +
  "in culpa qui officia deserunt mollit anim id est laborum.\n\n" +
  "Sed ut perspiciatis unde omnis iste natus error sit voluptatem accusantium " +
  "doloremque laudantium, totam rem aperiam, eaque ipsa quae ab illo inventore " +
  "veritatis et quasi architecto beatae vitae dicta sunt explicabo.\n\n" +
  "Nemo enim ipsam voluptatem quia voluptas sit aspernatur aut odit aut fugit, " +
  "sed quia consequuntur magni dolores eos qui ratione voluptatem sequi nesciunt.";

// Per-session state: sessionId -> { mode, model }
const sessionState = new Map();

const PLAN_STEPS = [
  { content: "1. Analyze the request", status: "completed" },
  { content: "2. Generate response", status: "in_progress" },
  { content: "3. Deliver result", status: "pending" },
];

function send(obj) {
  console.log(JSON.stringify(obj));
}

// ACP ConfigOption schema: id, name, description?, category, type, currentValue, options
// ACP ConfigOptionValue: value, name, description?
function buildConfigOptions(sessionId) {
  const s = sessionState.get(sessionId) ?? { mode: "echo", model: "a" };
  return [
    {
      id: "mode",
      name: "Session Mode",
      description: "Test case to run on prompt",
      category: "mode",
      type: "select",
      currentValue: s.mode,
      options: MODE_VALUES.map((value) => ({ value, name: value })),
    },
    {
      id: "model",
      name: "Model",
      description: "Model selector",
      category: "model",
      type: "select",
      currentValue: s.model,
      options: MODEL_VALUES.map((value) => ({ value, name: value })),
    },
  ];
}

// ACP SessionMode: id, name, description? — kept in sync with mode config per spec
function buildSessionModes() {
  return MODE_VALUES.slice(0, 5).map((id) => ({
    id,
    name: id,
    description: `Run ${id} test case`,
  }));
}

function getSessionState(sessionId) {
  if (!sessionState.has(sessionId)) {
    sessionState.set(sessionId, { mode: "echo", model: "a" });
  }
  return sessionState.get(sessionId);
}

function sendUpdate(sessionId, update) {
  send({
    jsonrpc: "2.0",
    method: "session/update",
    params: { sessionId, update: { sessionUpdate: update.sessionUpdate, ...update } },
  });
}

function sendChunk(sessionId, text) {
  sendUpdate(sessionId, {
    sessionUpdate: "agent_message_chunk",
    content: { type: "text", text },
  });
}

function sendPlan(sessionId, entries) {
  sendUpdate(sessionId, {
    sessionUpdate: "plan",
    entries: entries.map((e) =>
      typeof e === "string" ? { content: e } : { content: e.content, priority: e.priority, status: e.status }
    ),
  });
}

function sendConfigOptionUpdate(sessionId, opts) {
  sendUpdate(sessionId, { sessionUpdate: "config_option_update", configOptions: opts });
}

function sendCurrentModeUpdate(sessionId, modeId) {
  sendUpdate(sessionId, { sessionUpdate: "current_mode_update", modeId });
}

function sendToolCall(sessionId, toolCallId, title, status = "running") {
  sendUpdate(sessionId, {
    sessionUpdate: "tool_call",
    toolCallId,
    title: title || "read_file",
    status,
  });
}

function sendToolCallUpdate(sessionId, toolCallId, status, content) {
  const upd = { sessionUpdate: "tool_call_update", toolCallId };
  if (status) upd.status = status;
  if (content) upd.content = content;
  sendUpdate(sessionId, upd);
}

function streamSync(sessionId, text, chunkSize = 1) {
  for (let i = 0; i < text.length; i += chunkSize) {
    sendChunk(sessionId, text.slice(i, i + chunkSize));
  }
}

function streamAsync(sessionId, text, chunkSize, delayMs, requestId, done) {
  CANCELLED_SESSIONS.delete(sessionId);
  let i = 0;
  function next() {
    if (CANCELLED_SESSIONS.has(sessionId)) {
      CANCELLED_SESSIONS.delete(sessionId);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "cancelled" } });
      done();
      return;
    }
    if (i >= text.length) {
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      done();
      return;
    }
    sendChunk(sessionId, text.slice(i, Math.min(i + chunkSize, text.length)));
    i += chunkSize;
    setTimeout(next, delayMs);
  }
  next();
}

function handleInitialize(params) {
  send({
    jsonrpc: "2.0",
    id: params._id,
    result: {
      protocolVersion: 1,
      agentCapabilities: {
        loadSession: false,
        promptCapabilities: { image: false, audio: false, embeddedContext: false },
      },
      agentInfo: { name: "mock-echo", version: "0.3" },
    },
  });
}

function handleSessionNew(params) {
  const sessionId = "mock-session-" + Date.now();
  sessionState.set(sessionId, { mode: "echo", model: "a" });
  send({
    jsonrpc: "2.0",
    id: params._id,
    result: {
      sessionId,
      sessionModes: buildSessionModes(),
      configOptions: buildConfigOptions(sessionId),
    },
  });
}

// ACP session/set_config_option params: sessionId, configId, value
function handleSetConfigOption(params) {
  const { sessionId, configId, value } = params;
  const s = getSessionState(sessionId);
  if (configId === "mode" && MODE_VALUES.includes(value)) {
    s.mode = value;
  } else if (configId === "model" && MODEL_VALUES.includes(value)) {
    s.model = value;
  }
  send({
    jsonrpc: "2.0",
    id: params._id,
    result: { configOptions: buildConfigOptions(sessionId) },
  });
}

// ACP session/prompt params: sessionId, prompt (ContentBlock[])
function handleSessionPrompt(params) {
  const { sessionId, prompt: promptBlocks } = params;
  const textBlock = (promptBlocks || []).find((p) => p.type === "text");
  const userText = (textBlock ? textBlock.text : "").trim();
  const requestId = params._id;

  const parts = userText.split(/\s+/);
  const firstWord = (parts[0] || "").toLowerCase();
  const rest = parts.slice(1).join(" ") || "hello";

  const s = getSessionState(sessionId);
  const cmd = MODE_VALUES.includes(firstWord) ? firstWord : s.mode;

  switch (cmd) {
    case "echo":
      streamSync(sessionId, `Echo: ${userText || "(no text)"}`, 1);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      return;
    case "long":
      streamSync(sessionId, `You said: "${rest}"\n\n${LOREM}`, 6);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      return;
    case "big":
      streamSync(sessionId, `Echo: "${rest}"\n\n${LARGE_BODY}`, 8);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      return;
    case "slow":
      streamAsync(
        sessionId,
        `Streaming: "${rest}"\n\n${LARGE_BODY}`,
        10,
        45,
        requestId,
        () => {}
      );
      return;
    case "error":
      send({
        jsonrpc: "2.0",
        id: requestId,
        error: { code: -32603, message: "Internal error (mock)", data: { detail: rest || "simulated" } },
      });
      return;
    case "refusal":
      streamSync(sessionId, `Refusing: ${rest}\n`, 1);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "refusal" } });
      return;
    case "max_tokens":
      streamSync(sessionId, `Truncated: ${rest}\n`, 1);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "max_tokens" } });
      return;
    case "plan": {
      sendPlan(sessionId, [
        { content: PLAN_STEPS[0].content, status: "completed" },
        { content: PLAN_STEPS[1].content, status: "in_progress" },
        { content: PLAN_STEPS[2].content, status: "pending" },
      ]);
      setTimeout(() => {
        sendPlan(sessionId, [
          { content: PLAN_STEPS[0].content, status: "completed" },
          { content: PLAN_STEPS[1].content, status: "completed" },
          { content: PLAN_STEPS[2].content, status: "in_progress" },
        ]);
        setTimeout(() => {
          sendPlan(sessionId, [
            { content: PLAN_STEPS[0].content, status: "completed" },
            { content: PLAN_STEPS[1].content, status: "completed" },
            { content: PLAN_STEPS[2].content, status: "completed" },
          ]);
          send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
          setTimeout(() => {
            sendPlan(sessionId, []);
          }, 5000);
        }, 1500);
      }, 1500);
      return;
    }
    case "tool":
      const tcId = "tc-" + Date.now();
      const filePath = rest || "file.txt";
      sendToolCall(sessionId, tcId, "read_file", "running");
      setTimeout(() => {
        const completed = Math.random() < 0.5;
        const status = completed ? "completed" : "failed";
        sendToolCallUpdate(sessionId, tcId, status, [{ type: "text", text: filePath }]);
        streamSync(
          sessionId,
          completed ? `Tool completed.\n` : `Tool failed.\n`,
          4
        );
        send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      }, 80);
      return;
    case "config":
      sendConfigOptionUpdate(sessionId, [
        ...buildConfigOptions(sessionId),
        {
          id: "temp",
          name: "Temp",
          description: "Debug option",
          category: "_debug",
          type: "select",
          currentValue: "a",
          options: [{ value: "a", name: "A" }, { value: "b", name: "B" }],
        },
      ]);
      streamSync(sessionId, "Pushed config_option_update.\n", 1);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      return;
    case "mode": {
      const s = getSessionState(sessionId);
      s.mode = "slow";
      sessionState.set(sessionId, s);
      sendCurrentModeUpdate(sessionId, "slow");
      streamSync(sessionId, "Switched to slow mode.\n", 1);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      return;
    }
    case "help":
      streamSync(
        sessionId,
        "Modes: " + MODE_VALUES.join(", ") + ". Type as first word to override.",
        6
      );
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
      return;
    default:
      streamSync(sessionId, `Echo: ${userText || "(no text)"}`, 1);
      send({ jsonrpc: "2.0", id: requestId, result: { stopReason: "end_turn" } });
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;

  try {
    const msg = JSON.parse(trimmed);
    const id = msg.id;
    const method = msg.method;
    const params = { ...(msg.params || {}), _id: id };

    if (method === "initialize") {
      handleInitialize(params);
    } else if (method === "session/new") {
      handleSessionNew(params);
    } else if (method === "session/prompt") {
      handleSessionPrompt(params);
    } else if (method === "session/set_config_option") {
      handleSetConfigOption(params);
    } else if (method === "session/cancel") {
      if (params.sessionId) CANCELLED_SESSIONS.add(params.sessionId);
    } else if (id !== undefined && id !== null) {
      send({
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: "Method not found" },
      });
    }
  } catch (e) {
    console.error("mock-agent parse error:", e.message);
  }
});
