// workspace.* contract methods (M8 phase 1). Browse reads PG state so it works
// while the host is down.
import * as workspaces from "../workspaces.ts";
import { requireString, type MethodTable } from "./common.ts";

export function workspaceMethods(): MethodTable {
	return {
		async "workspace.list"(_conn, params) {
				return workspaces.workspaceList(requireString(params, "sessionId"));
			},
			async "workspace.list_dir"(_conn, params) {
				return workspaces.workspaceListDir(requireString(params, "sessionId"), params);
			},
			async "workspace.read_file"(_conn, params) {
				return workspaces.workspaceRead(requireString(params, "sessionId"), params);
			},
			async "workspace.write_file"(_conn, params) {
				return workspaces.workspaceWrite(requireString(params, "sessionId"), params);
			},
			async "workspace.search"(_conn, params) {
				return workspaces.workspaceSearch(requireString(params, "sessionId"), params);
			},
			async "workspace.git_status"(_conn, params) {
				return workspaces.workspaceGitStatus(requireString(params, "sessionId"), params);
			},
			async "workspace.git_diff"(_conn, params) {
				return workspaces.workspaceGitDiff(requireString(params, "sessionId"), params);
			},
	};
}
