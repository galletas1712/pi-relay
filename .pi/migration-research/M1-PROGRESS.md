# M1 PROGRESS LOG

- [x] Research read (INDEX/seams/plan, PA kernel+rlm sources, upstream sdk/args/rpc-mode/loader/types)
- [x] `extensions/prime-rlm/` scaffolded: kernel port, provisioner, ipython tool, rlm host, registry, prompt, python runtime pkg
- [x] `.pi/m1-demo/` scaffolded: pinned npm install of @earendil-works/pi-coding-agent@0.84.1, agentDir (settings+models), driver, runner
- [x] Extension typecheck clean (tsc)
- [x] Kernel venv provisioned (uv, py3.12, ipykernel, prime-rlm-runtime 0.1.1)
- [x] BLOCKER DIAGNOSED: NVIDIA endpoint returns empty SSE streams for these models; non-streaming OK → built `.pi/m1-demo/shim-proxy.mjs` (demo harness, not a pi patch)
- [x] V1 PASS (Python exec; SQUARES_SUM 285)
- [x] V2 PASS (%%bash; BASH_TOKEN_42)
- [x] V4 PASS (kernel persistence; PERSIST_57 across separate calls)
- [x] V3 initially FAILED on bridge envelope collision (`status:"completed"` clobbered `status:"ok"`); GLM self-debugged and hot-patched the kernel runtime mid-run
- [x] Fixed (nest payload under `value`, runtime 0.1.1, venv reinstalled); V3b PASS (child completed, result 144, registry 1 entry)
- [x] V5 PASS (depth guard error path; RLM_MAX_DEPTH=0)
- [x] Report: `.pi/migration-research/M1-DEMO.md`
