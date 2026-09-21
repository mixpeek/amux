# Project lifecycle

Amux turns one requested outcome into independently verified changes. The durable hierarchy is:

```text
Project intent + acceptance contract
  -> same-project task graph
  -> disposable task attempts in isolated worktrees
  -> exact reports, checks, commits and retained assets
  -> deterministic verification and integration
  -> whole-project checks on current origin/main
  -> human artifact review
  -> executor expiration and worktree removal
```

Projects own intent, dependencies and final acceptance. Tasks own one verifiable unit of work.
Workers are attempts: they may fail or be replaced without changing task identity. A worker cannot
approve its own task, change a project verifier, create a dependency in another project, or declare
the project accepted.

## Review and disposal

Every new project starts with a human artifact-review criterion. A project may also declare bounded
command criteria. Command verifiers run once per fingerprint of contract revision, current intent
and current `origin/main`. Their receipts and the human decision are append-only.

Once a task is integrated and verified, its executor is stopped. This consumes no model tokens. Its
worker record, terminal history, worktree and content-addressed report assets remain available. The
shared retirement function refuses to remove them until a human criterion exists and the exact
current project fingerprint is Accepted. Projects created before this rule show **Review gate needs
configuration** and retain their executors until the contract is fixed.

Approval releases cleanup. The normal ephemeral sweep then confirms the worker board is fully
Verified, its reported head is contained in current remote main, the checkout is clean and unchanged,
and only then removes the worktree and marks the worker Expired. A rejection retains the evidence and
executor context for a follow-up request.

This follows the useful boundary in Factory's delegation flow: autonomous execution returns a
reviewable session and change set, while proposed changes and test results remain subject to review
before the lifecycle is considered delivered. See Factory's
[Remote Delegations](https://docs.factory.ai/remote-delegations) and
[Automated Code Review](https://docs.factory.ai/software-factory/code-review-ci) documentation.

## Guarantees

Amux can guarantee the following auditable statement:

> Every declared criterion was evaluated by its configured verifier against the recorded current
> inputs, all required human decisions were explicitly recorded, and disposal happened only after
> that acceptance remained current.

It cannot prove that an undeclared requirement is correct. The acceptance contract and produced
artifacts are therefore the human review boundary, rather than a model's claim that it is done.
