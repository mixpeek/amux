---
description: Validate features end-to-end through real user simulation, browser interaction, measurable outcomes, visual inspection, and evidence-backed acceptance criteria.
allowed-tools: Bash, Read, Edit
argument-hint: [feature or user journey]
amux-managed: true
---

# E2E Reality Test

The user's request is: **$ARGUMENTS**

## Objective

Prove that the requested functionality works as a real user would experience it. Passing unit tests, API responses, or successful builds is insufficient.

**Test observable outcomes, not implementation claims.**

## Protocol

1. **Define success**
   - Identify the intended user, goal, and expected outcome.
   - Establish measurable acceptance criteria before testing.
   - Include functional correctness, usability, reliability, latency, and visual quality.

2. **Prepare reality**
   - Start the actual application and its dependencies.
   - Use a real browser via Playwright or available browser automation.
   - Exercise the real frontend, backend, authentication, persistence, and integrations.
   - Use realistic accounts, permissions, and representative data.
   - Never replace critical paths with mocks unless explicitly disclosed.

3. **Simulate the user**
   - Navigate from the actual entry point.
   - Interact through visible UI controls rather than internal APIs.
   - Complete the entire journey without privileged shortcuts.
   - Test happy paths, failures, edge cases, refreshes, retries, and recovery.
   - Verify results persist and appear correctly to the user.

4. **Observe and measure**
   - Capture screenshots, browser traces, console errors, network failures, and relevant server logs.
   - Measure completion rate, elapsed time, interaction count, latency, and errors.
   - Compare actual outcomes against acceptance criteria.
   - Inspect desktop and mobile layouts when relevant.

5. **Judge like a human**
   - Review screenshots and interaction sequences.
   - Evaluate clarity, intuitiveness, friction, feedback, accessibility, and visual consistency.
   - Ask: Could an unfamiliar user complete this without instructions?
   - Identify confusing interactions, misleading states, and unnecessary steps.
   - Score functionality, usability, reliability, performance, and visual quality from 0–10.

6. **Adversarial validation**
   - Attempt to break the journey with invalid inputs, interrupted requests, double submissions, slow networks, and unexpected navigation.
   - Test relevant permission boundaries and concurrent actions.
   - Confirm failures are handled gracefully without data corruption or false success messages.

7. **Fix and repeat**
   - Diagnose failures using captured evidence.
   - Fix issues at their root cause.
   - Rerun the affected journey and relevant regression tests.
   - Repeat until all