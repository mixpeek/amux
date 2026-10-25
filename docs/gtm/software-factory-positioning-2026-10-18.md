# Should amux lead with "build your own software factory"?

Research for the homepage, 2026-10-18. **Recommendation: no, not as the headline.** Keep "Run an AI engineering team" and add a section that shows the linked-history loop with one real example.

## The term is taken

- Factory's homepage H1 is "Build Your Software Factory" ([factory.com](https://factory.com/)). They raised a $150M Series C at about $1.5B ([Sacra](https://sacra.com/c/factory/)) and already pitch "a self-improving system for your SDLC" ([Hashnode](https://hashnode.com/blog/ai-software-factory)). No trademark record was found, but USPTO has not been checked.
- Warp leads with "Open infrastructure for cloud software factories" and "self-improvement built in" ([warp.dev](https://www.warp.dev/)); Warp Factories launched 2026-08-18 ([Fortune](https://fortune.com/press-releases/warp-launches-warp-factories-automate-software-development-2026-08-18/)).
- 8090, StrongDM, BCG ("dark software factory") and Microsoft ("Agent Factory") use the same language ([Simon Willison](https://simonwillison.net/2026/Feb/7/software-factory), [BCG Platinion](https://bcgplatinion.com/insights/the-dark-software-factory)).

## Developers are cooling on it

- A Pieter Levels post mocking "software factory" builders drew about 364K views with mostly negative replies ([Digg](https://digg.com/tech/f5f7849f-0d6a-4d63-b94e-f54d701908b8)).
- "Why Software Factories Fail" (AI Engineer World's Fair 2026) argues lights-off factories do not work ([ai.engineer](https://ai.engineer/talks/harness-engineering-is-not-enough-why-software-factories-fail)); PostHog says the hype stalled afterwards ([PostHog](https://posthog.com/newsletter/software-factories)).
- Addy Osmani warns about "comprehension debt" in dark factories ([addyosmani.com](https://addyosmani.com/blog/software-factories/)).
- To developers the word now suggests no human review. amux keeps a human on the board, so the term contradicts the product. Search volume was not measured.

## Competitor heroes

| Product | Hero |
|---|---|
| Factory | Build Your Software Factory |
| Warp | Open infrastructure for cloud software factories |
| Devin | the first autonomous software engineer |
| Conductor | Run parallel coding agents on your Mac |
| Claude Squad | Manage multiple AI terminal agents |
| Sculptor | the missing UI for parallel coding agents |
| Omnara | The open-source alternative to Claude Managed Agents |
| OpenHands | open source platform for software engineering agents |
| amux | Run An AI Engineering Team |

## "Self-improving from linked history"

This claim alone is not unique: Factory and Warp say "self-improving", and Warp documents a self-improvement agent role ([Warp docs](https://docs.warp.dev/platform/software-factory/)). What amux can show concretely is one self-hosted store where messages, commits, board cards, schedules and logs link to each other and to the agent that produced them. That lands as a worked example (a frustration entry that became a card, then a fix commit, then a guard), and reads as generic as an abstract claim.

## Options

Hero options that keep the current promise and add memory:

1. **Run an AI engineering team.** Claude Code, Codex and Gemini CLI agents in parallel on one board. Every message, commit and task is linked, so the next run starts from what the last one learned.
2. **Your agents, with a memory.** Run coding agents in parallel. amux links their messages, commits, board cards and schedules into one history they can search and build on.
3. **Agents that ship, and remember why.** Shared board, messages and schedules, all traced back to the commit.

Use "software factory" at most once in body copy for search.
