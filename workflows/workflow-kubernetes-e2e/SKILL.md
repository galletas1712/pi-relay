---
name: workflow-kubernetes-e2e
description: Run an adaptive end-to-end test against a Kubernetes cluster as a single full delegation, with cluster-safety rules. Use for operator/e2e validation where the agent must observe rollout and classify the outcome.
---

# Workflow: kubernetes e2e

Run an adaptive end-to-end test against a Kubernetes cluster as a single full
delegation. The subagent deploys/observes/collects evidence and classifies the
result. You decide what to do with that result.

## Delegations
- tester — full subagent (runs cluster ops; writes evidence files into the
  workspace and reports their paths). The generic `tester` role carries the
  kubernetes specifics in its delegation prompt.

## Outcomes (suggested_next, in index.json)
- tester: pass | product_failure | environment_retry | human_needed

## Control flow
1. tester (a full delegation; put the cluster-safety rules below in its prompt)
2. On its outcome:
   - pass             -> DONE
   - product_failure  -> the cluster behaved but the product is wrong: stop and
                         report; if there is code to fix, hand off to
                         implement_review_test, then return here.
   - environment_retry-> a transient/infra issue (e.g. scheduling): re-run the
                         tester once; if it recurs, ask the human.
   - human_needed     -> auth expiry, an unsafe/destructive op, or an ambiguous
                         decision: relay to the human and wait.

## Running the delegation (one delegation per turn, then end your turn)
- test: delegate_writing_task({ role: "tester",
    prompt: "<context, namespace, what to deploy/test, what 'pass' means>",
    workflow: "kubernetes_e2e" })

The `tester` role is generic; put all kubernetes-specific guidance below in the
delegation prompt, and REQUIRE the tester to end its final message with a line
`suggested_next: pass | product_failure | environment_retry | human_needed`.

Cluster-safety rules to put in the tester's prompt:
- Always pass an explicit `--context` and namespace; never rely on the current
  kube context.
- Avoid destructive cluster-scoped operations; request human approval for
  dangerous shared-cluster actions.
- On auth/Teleport expiry, return `human_needed` rather than guessing.
- Collect logs/events/manifests as evidence files in the workspace and list
  their paths in the final message (these persist because it is a full delegation).
