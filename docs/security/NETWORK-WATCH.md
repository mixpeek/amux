# Network watch

`scripts/network_watch.py`, run by the amux-security lane as a `kind: shell`
schedule every 10 minutes with `--fix`. A quiet run costs no model call. It is a
companion to the advisory watch in [RED-TEAM-LOOP.md](RED-TEAM-LOOP.md):
that one watches what amux is built from, this one watches what the host exposes
and who uses it. Requested in MSG-104254.

## What it checks

| check | source | anomaly |
|---|---|---|
| listeners | `lsof -iTCP -sTCP:LISTEN` | a TCP listener on a non-loopback address that is not in the baseline. A cargo test binary (`target/<profile>/deps/<crate>-<16 hex>`) takes new random ports each run, so it is reported once per binary per day rather than once per port |
| file servers | listener command line | `python -m http.server` and similar on a non-loopback address. **Stopped automatically** |
| tailnet | `tailscale status --json` | a new node, a node owned by another user, a node shared in |
| exposure | `tailscale serve status --json` | any serve or funnel config |
| requests | the server's `_amux_request_log`, read-only | a client that is neither loopback nor a tailnet node; 60 or more 401/403 from one client in one window |
| firewall | `socketfilterfw --getglobalstate` | the state changed from the baseline |
| egress | `lsof -iTCP -sTCP:ESTABLISHED` | a process name with public connections not seen before; a shell or raw-socket tool (bash, nc, socat, ...) alerts per new destination outside GitHub's range and is never accepted by name |

The first run records the baseline. Standing exposure (a foreign or shared node,
a serve config, a wide file server) is reported even on that run. After an
anomaly is accepted as normal, `python3 scripts/network_watch.py --rebaseline`
records the current snapshot. State lives in
`~/.amux/tmp/amux-security/network-watch/`.

It alerts the amux-security lane only on an anomaly key it has not seen, and an
anomaly that clears and comes back alerts again. The message carries addresses,
ports, counts and process names only, cleaned to a short charset, because request
fields are attacker-controlled and the message lands in an agent's turn.

Fail closed: exit 2 when a probe cannot run (state untouched), exit 3 when the
message was not delivered. The scheduler reports a non-zero exit to the lane.
`scripts/test-network-watch.py` (31 checks, macOS) proves each class fires, the
auto-fix stops a real wide file server, and every probe failure exits 2.

## Why only one auto-fix

A wrong automatic action on a shared host breaks every worker, so an action is
automatic only when it is always wrong and never load-bearing. An ad-hoc static
file server on a wide address qualifies: the owner's rules forbid starting one,
and one ran for five days serving a deleted directory before this watch existed.
Blocking an address, changing the firewall, editing tailnet ACLs or killing any
other listener needs root or the owner, so those are reported.

## What it cannot see

- Short-lived connections between samples, UDP egress, and DNS. Seeing those
  needs a network extension (LuLu, Little Snitch) or pf logging, both of which
  need the owner to install or grant root.
- Exfiltration through an allowed host, including the model API itself. Egress
  rules do not cover that; tool-call inspection does.
- Requests that never reach the request log (refused at TLS, or a different
  listener).

## Controls that need the owner

Ranked for an agent host where every worker runs as one user:

1. Turn on the macOS application firewall with stealth mode, so LAN peers reach
   only what is allowed. Needs an admin password.
2. Bind the amux server to loopback and the tailnet address instead of every
   interface, so LAN peers cannot reach it at all. A server change.
3. Per-address lockout after repeated auth failures, in the server. Today the
   watch reports bursts and nothing slows them.
4. Default-deny egress for agent processes with an allowlist, which is the
   common recommendation for AI agent hosts (NVIDIA AI Red Team; Stride). On
   macOS that means a network extension. Needs the owner to install and approve it.
5. Tailnet ACLs that let only the owner's devices reach port 8824.

Sources:
[NVIDIA, four ways to deploy more secure AI agents](https://developer.nvidia.com/blog/four-ways-to-deploy-more-secure-ai-agents/),
[Stride, network egress control for AI agents](https://www.stride.build/blog/network-egress-control-ai-agents),
[PromptArmor, what domains should I add to my allowlist](https://www.promptarmor.com/resources/what-domains-should-i-add-to-my-allowlist),
[pf anchors on macOS](https://inventivehq.com/knowledge-base/macos/how-to-configure-macos-firewall-pf).
