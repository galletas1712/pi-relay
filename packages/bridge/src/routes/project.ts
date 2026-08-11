// project.* contract methods (M8 phase 1): projects group sessions and own
// workspace-bases/<project>/<dir>/ materialization.
import * as db from "../db.ts";
import * as projects from "../projects.ts";
import { checkIdem, requireString, type MethodTable } from "./common.ts";

export function projectMethods(): MethodTable {
	return {
		async "project.list"() {
				return { projects: await projects.listProjects() };
			},
			async "project.create"(_conn, params) {
				const name = requireString(params, "name");
				const decls = params.workspaces !== undefined ? projects.parseWorkspaceDecls(params.workspaces) : [];
				const key = params.idempotencyKey !== undefined ? requireString(params, "idempotencyKey") : undefined;
				const replay = await checkIdem(key, "project.create", { name, workspaces: decls });
				if (replay) return { ...(replay.replay as Record<string, unknown>), replay: true };
				const res = await projects.createProject(name, decls);
				if (key) {
					await db.idemPut({
						key,
						method: "project.create",
						paramsHash: db.paramsHash({ name, workspaces: decls }),
						response: res,
					});
				}
				return res;
			},
			async "project.update"(_conn, params) {
				const id = requireString(params, "id");
				const name = params.name !== undefined ? requireString(params, "name") : undefined;
				const decls = params.workspaces !== undefined ? projects.parseWorkspaceDecls(params.workspaces) : undefined;
				return projects.updateProject(id, { name, workspaces: decls });
			},
			async "project.delete"(_conn, params) {
				return projects.deleteProject(requireString(params, "id"));
			},
		};
}
