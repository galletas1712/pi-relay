import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import type { Plugin } from "vite";
import { handleGithubProxyRequest } from "./server/githubProxy.ts";

export function githubProxyPlugin(): Plugin {
	const attach = (middlewares: Connect.Server) => {
		middlewares.use((req, res, next) => {
			void handleGithubProxyRequest(req as IncomingMessage, res as ServerResponse).then((handled) => {
				if (!handled) next();
			});
		});
	};

	return {
		name: "pi-relay-github-proxy",
		configureServer(server) {
			attach(server.middlewares);
		},
		configurePreviewServer(server) {
			attach(server.middlewares);
		},
	};
}
