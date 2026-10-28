---
description: Iteratively improve an artifact through scored evaluation until a target quality threshold is reached.
allowed-tools: Bash, Read, Edit, Write
argument-hint: [iterations=5] [target=9] [criteria=auto] [task]
amux-managed: true
---

# Iterative Improvement

The user's request is: **$ARGUMENTS**

## Instructions

1. **Establish baseline:** Inspect the artifact and define 3–5 measurable quality criteria relevant to the task (e.g., correctness, simplicity, performance, maintainability, UX).
2. **Score:** Rate each criterion 1–10, calculate an overall weighted score, and identify the highest-impact weaknesses.
3. **Improve:** Make concrete changes targeting the largest weaknesses. Follow repository conventions and preserve existing functionality.
4. **Verify:** Run relevant tests, checks, or evaluations. Score again using the same rubric and evidence.
5. **Repeat:** Continue until the target score is reached or the maximum iteration count is exhausted.

## Rules

- Default to 5 iterations and a target score of 9/10.
- Prioritize high-impact improvements over cosmetic changes.
- Each iteration must produce a meaningful improvement or a documented finding.
- Never inflate scores; justify ratings with evidence.
- Do not regress previously passing criteria.
- If an iteration fails to improve the score, change approach.
- Stop early when the target is reached and verification passes.
- Do not claim success when tests fail or evidence is insufficient.

## Output

Report a concise iteration history:

| Iteration | Score | Changes | Remaining Gaps |
|---|---|---|---|
| 0 | X/10 | Baseline | ... |
| 1 | X/10 | ... | ... |

**Final:** Score, verification results, remaining gaps, and whether the target was achieved.