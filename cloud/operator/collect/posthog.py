#!/usr/bin/env python3
"""PostHog: visits, pricing views and Cloud funnel events over 30 days (plan 6)."""
import _common as c

key = c.env("POSTHOG_PERSONAL_API_KEY")
PROJECT = c.env("POSTHOG_PROJECT_ID") or "378145"
Q = """select
  countIf(event = '$pageview' and properties.$host = 'amux.io') as views,
  uniqIf(person_id, event = '$pageview' and properties.$host = 'amux.io') as visitors,
  uniqIf(person_id, event = '$pageview' and properties.$pathname like '/pricing%') as pricing_viewers,
  countIf(event = 'cloud.notify_signup') as waitlist_signups
from events where timestamp > now() - interval 30 day"""


def collect():
    r = c.fetch(f"https://us.posthog.com/api/projects/{PROJECT}/query/", {"Authorization": f"Bearer {key}"},
                {"query": {"kind": "HogQLQuery", "query": Q}})
    row = (r.get("results") or [[None] * 4])[0]
    return dict(zip(["views_30d", "visitors_30d", "pricing_viewers_30d", "waitlist_signups_30d"], row))


if not key:
    c.write("posthog", why_unmeasured="POSTHOG_PERSONAL_API_KEY unset")
else:
    c.run("posthog", collect)
