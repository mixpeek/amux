# Message delivery during connection failures

The dashboard keeps each ordinary message locally before attempting delivery. Recovery uses the same message identity; a missing HTTP response does not mean the worker missed the text. Server acceptance and delivery to the CLI are separate facts.

## Fixed in this change

- An unavailable transcript or ten minutes of receipt polling no longer permanently hands recovery back to the user. Receipt reads continue with the existing 2–60 second backoff. Previously timed-out entries resume automatically.
- Pending reservations switch to receipt reads instead of repeatedly posting. An absent reservation explicitly permits a retry through the same identity gate.
- A concurrent retry no longer removes the original request's in-flight ownership. Ownership is reference-counted until every request ends.
- Confirmed message identities no longer expire after 30 days and allow an old device to repeat accepted input.
- Codex can reconcile a lost receipt using timestamped, exact user text in its own identified rollout. Assistant text, earlier messages, partial matches and missing evidence cannot authorize a resend.

## Measured validation

The actual dashboard replay functions run under Node with a real TCP fixture that closes its listening socket, restarts, drops the response after acceptance, and returns transient 503 responses. Queue data is serialized/reloaded between attempts. Two message identities reach the fixture exactly once, in FIFO order, and the queue empties. This is transport fault simulation, not a production-server shutdown.

The live replay test targets `status-hooks-luna` on main/8824, using gpt-6-luna with low reasoning. It injects connection failure before sending, reloads durable queue data, discards an actual successful server response, disconnects again, and reconnects. Two direct messages and two server-queued steering messages were accepted. The provider's canonical user records contain all four exactly once; both steering rows reached `sent`, with no pending server rows. The isolated worker received literal text. The final audit then caught a separate defect: the delivery tick created two board cards for explicit owner steering despite isolation. The fix moves delivered-message intake behind the same isolation boundary; its regression exercises that delivery-stage function, not just enqueue-time history. The two test-only cards are retained archived for diagnosis.

A separate send using the real dashboard composer appeared as Human/direct, MSG-68563, and the worker replied. The 389px mobile viewport had no document overflow. Browser screenshot capture clips part of the rendered viewport on this host, so this is not a claim of exhaustive visual certification.

Evidence: [direct delivery](evidence/message-recovery-20260923/live-transport.json), [steering](evidence/message-recovery-20260923/live-steering.json), [provider records](evidence/message-recovery-20260923/provider-message-counts.json).

Run `node --test e2e/outbox-acceptance-recovery.test.mjs e2e/pending-message-projection.test.mjs`: 20 passed. Five of the new recovery assertions fail against the pre-change dashboard source (10 pass / 5 fail), and all 15 replay tests pass with the fix. `npm run test:state`: 27 passed. `scripts/spa-lint.sh`: zero errors, 51 existing warnings.

## Boundaries

A server receipt confirms terminal submission or durable server queuing; the provider transcript is the additional proof of delivery used above. Actual provider processing is not implied by HTTP 200 alone. If a crash destroys every reliable delivery record, the client retains the message and continues checking; it must not claim delivery or blindly paste a duplicate. Authorization refusals and unsupported operations still require an explicit remedy. No production fleet outage, credential change or hook trust grant was used for these tests.
