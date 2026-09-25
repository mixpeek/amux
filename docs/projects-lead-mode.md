# Projects: one lead, one outcome

New Projects use one persistent lead worker and, by default, one Git worktree. The lead can use provider-native tools and temporary subagents inside its own session. Amux does not create a durable worker, lease, or dependency for every plan step. The lead writes its current plan and progress to `.amux/project-lead.json`; Amux journals changes and shows them in the Plan tab. Plan steps are observations, not dispatch authority.

The durable inputs are the original and subsequent project requests, the repository and checkout, the worker transcript, the acceptance contract, and retained evidence. Each request has an idempotent receipt and is delivered to the same lead. A new request invalidates an older candidate. A server restart reconstructs the run from those records and resumes the same worker. Pause stops the worker.

`ready_for_verification` in the lead's progress file only proposes a committed, clean candidate. The progress file is Git-ignored; the server measures the checkout's actual HEAD rather than trusting a SHA written by the agent. It runs the configured whole-project checks independently and retains the candidate's changed passive files for review. A failed check returns the measured failure to the lead for repair. Automated checks must pass before human artifact review; human approval binds to the candidate fingerprint. Only approved closeout publishes to main, expires the lead, removes the worktree, and retains the transcript and evidence.

The lead may request input, spend approval, customer-outbound approval, or report an external blocker. These are explicit states. Ordinary implementation choices and other workers are not project dependencies. Configured usage budgets stop new lead turns at the observed boundary.

A published project is terminal. Its plan, lead transcript, and retained files remain reviewable; another outcome starts as a new project. Before publication, additional requests or revised criteria update the same project's intent and invalidate any older candidate.

Existing task-driven project records keep their execution mode so their in-flight work and evidence remain readable. The new-project form creates lead projects. A task-driven project with recorded work cannot silently switch modes because its old leases and candidate receipts have different ownership semantics.
