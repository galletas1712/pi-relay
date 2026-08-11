import { BridgeClient } from "/home/schwinns/pi-relay/packages/bridge/test/client.mjs";
const c = new BridgeClient("ws://127.0.0.1:8730", "mMNGDZb4Gerl9cJQcCnZz3fKPGShHpN", "http://localhost:3000",
  "/home/schwinns/pi-relay/packages/migrator/m10a-v4-trace.jsonl");
await c.connect();
const res = await c.call("subagent.tree", { sessionId: "3a4df093-5af4-559b-8dd0-a04b4ca0e2e7" });
console.log(JSON.stringify(res.result ?? res, null, 1));
process.exit(0);
