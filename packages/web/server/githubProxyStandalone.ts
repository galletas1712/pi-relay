import { createServer } from "node:http";
import { handleGithubProxyRequest } from "./githubProxy.ts";

const port = Number(process.env.PI_GITHUB_PROXY_PORT || 8790);
const host = process.env.PI_GITHUB_PROXY_HOST || "127.0.0.1";

createServer(async (req, res) => {
	const handled = await handleGithubProxyRequest(req, res);
	if (!handled) {
		res.statusCode = 404;
		res.setHeader("Content-Type", "application/json; charset=utf-8");
		res.end(JSON.stringify({ error: "not_found" }));
	}
}).listen(port, host, () => {
	console.log(`pi-relay github proxy listening on http://${host}:${port}`);
});
