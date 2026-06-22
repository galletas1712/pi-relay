---
name: workflow-kubernetes-e2e
description: Run an adaptive end-to-end test against a Kubernetes cluster as a read-only tester fan-out, with cluster-safety rules. Use for operator/e2e validation where the agent must observe rollout and classify the outcome.
---

# Workflow: kubernetes e2e

Run an adaptive end-to-end test against a Kubernetes cluster as a read-only
tester fan-out. The subagent deploys/observes/collects evidence in its
disposable snapshot and classifies the result. You decide what to do with that
result.
Read-only here means read-only with respect to the parent workspace; the
requested cluster actions may still change the target namespace, subject to the
safety rules below.

## Delegations
- tester — read-only subagent (runs cluster ops from a disposable snapshot;
  any local evidence files it writes stay in that snapshot). The generic
  `tester` role carries the kubernetes specifics in its delegation prompt.

## Outcomes (suggested_next, in the delegation snapshot)
- tester: pass | product_failure | environment_retry | human_needed

## Control flow
1. tester (a read-only fan-out; put the cluster-safety rules below in its prompt)
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
- test: delegate_readonly_tasks({ tasks: [ { role: "tester",
    prompt: "<context, namespace, what to deploy/test, what 'pass' means>" } ],
    workflow: "kubernetes_e2e" })

The `tester` role is generic; put all kubernetes-specific guidance below in the
delegation prompt, and REQUIRE the tester to end its final message with a line
`suggested_next: pass | product_failure | environment_retry | human_needed`.
When the completion observation arrives, branch on the delivered snapshot; call
`inspect_delegation` only to refresh/recover state or inspect later/running.

Cluster-safety rules to put in the tester's prompt:
- Always pass an explicit `--context` and namespace; never rely on the current
  kube context.
- Avoid destructive cluster-scoped operations; request human approval for
  dangerous shared-cluster actions.
- On auth/Teleport expiry, return `human_needed` rather than guessing.
- Collect logs/events/manifests as evidence when useful. Prefer summarizing the
  important evidence in the final message; any local evidence files are written
  only inside the tester's disposable snapshot, so treat their paths as
  non-persistent debugging aids rather than files the parent workspace can keep.
