import { BridgeClient } from "/home/schwinns/pi-relay/packages/bridge/test/client.mjs";
const SID = "73aa1b79-a438-5423-948e-0fc63fcdad5e";
const c = new BridgeClient("ws://127.0.0.1:8730", "mMNGDZb4Gerl9cJQcCnZz3fKPGShHpN", "http://localhost:3000",
  "/home/schwinns/pi-relay/packages/migrator/out/../m10a-boot-trace.jsonl");
await c.connect();
await c.call("session.attach", { id: SID, fromSeq: 0 });
const st = await c.call("session.getState", { id: SID });
console.log("state:", JSON.stringify(st.result ?? st).slice(0, 300));
const head = c.headSeq(SID);
await c.call("prompt.send", { sessionId: SID, idempotencyKey: `m10a-boot-${Date.now()}`,
  text: "This session was migrated from an older system. Without using any tools, reply with exactly: M10A-BOOT-OK" });
await c.waitIdleAfter(SID, head, 180000, "m10a boot turn");
const deltas = c.events.filter(e => e.event === "message.delta").map(e => e.data?.text ?? e.data?.delta ?? "").join("");
console.log("delta-len:", deltas.length);
console.log("marker observed:", deltas.includes("M10A-BOOT-OK"));
console.log("tail:", JSON.stringify(deltas.slice(-200)));
process.exit(0);
