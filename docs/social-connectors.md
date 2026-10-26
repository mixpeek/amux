# Social connectors

amux posts to social platforms and reads each post's results back, so amux-gtm
can run the closed loop in amux-GTM `internal/gtm/amux/PLAN.md` without Buffer.
Code: `crates/amux-server/src/api/social.rs`, the `social-metrics` runtime job,
and `amux social` in the CLI.

## What each platform needs

| # | Platform | Status | Gate |
|---|---|---|---|
| 1 | YouTube | built (AA-42) | Admin console delegation (below). Uploads stay private until Google's API audit passes |
| 2 | Instagram | not built (AA-43) | @amux_io as a Business account linked to a Facebook Page, and a Meta app with publishing approved |
| 3 | TikTok | not built (AA-44) | Content Posting API. Posts are visible only to the account until TikTok audits the app |
| 4 | X | not built (AA-45) | API v2 posting needs a paid tier. Pricing and sign-up are Ethan's |
| 5 | LinkedIn | not built (AA-46) | Community Management API approval and an amux company page |

`GET /api/social` reports the same table live.

## Using it

```bash
amux social post youtube clip.mp4 --title "8 agents, one board" --caption-stdin <<'EOF'
Queue tasks at night, wake up to merged PRs.
amux.io/?source=youtube-ai-coding-while-you-sleep
EOF
amux social track youtube <video_id> --posted-at <epoch>   # a post made in the browser
amux social ls youtube                                      # every post, views per window
amux social stats <id>                                      # a live read
```

The post reply carries the platform's `post_id`, the URL, the privacy asked
for and the privacy the platform applied. The `social-metrics` job stores one
snapshot per post at 24h, 72h and 7d (`GET /api/social/posts`). A failed read
is stored with `measured: false` and its error, then retried.

YouTube metrics: views, likes and comments come live from the Data API.
Shares, watch minutes, average view duration and subscribers gained come from
the Analytics API, which lags one to three days, so an early snapshot reports
`sources.analytics.measured: false` rather than zeros. YouTube's APIs do not
expose thumbnail impressions, so `impressions` is null and views is the reach
figure.

## Settings (server.env)

| Key | Meaning |
|---|---|
| `AMUX_SOCIAL_YOUTUBE_ACCOUNT` | The account YouTube posts as when a request names none (`hello@amux.io`) |
| `GOOGLE_SA_KEY_FILE_AMUX_IO` | The service-account key delegated in the amux.io Workspace (the amux-gmail account) |
| `AMUX_SOCIAL_PUBLIC` | `1` lets a post go public. Unset, a post asking for public is uploaded private and the reply says so. Owner's switch |
| `AMUX_SOCIAL_METRICS_SECS` | Snapshot pass interval, default 600, `0` disables |

A Google user grant for hello@amux.io expires within hours under the amux.io
session policy, so YouTube uses domain-wide delegation, like Cloud mail.

## Ethan's steps for YouTube

1. **Delegate the YouTube scopes** (two minutes). admin.google.com for amux.io,
   Security, API controls, Domain-wide delegation. Edit client
   `105196980959858115796` (amux-gmail) and add these to its existing
   `gmail.send` scope:

   ```
   https://www.googleapis.com/auth/youtube.upload,https://www.googleapis.com/auth/youtube.readonly,https://www.googleapis.com/auth/yt-analytics.readonly
   ```

   After this, uploads and metrics work. Uploads stay private until step 2 passes.

2. **Submit the YouTube API audit.** Form: https://support.google.com/youtube/contact/yt_api_form
   ("YouTube API Services - Audit and Quota Extension Form"), signed in as an
   owner of GCP project `mixpeek-inference-463103`. Draft answers below.

The YouTube Data API v3 and YouTube Analytics API are both enabled on that
project (Analytics enabled 2026-10-25).

### Draft audit answers

- **API client name:** amux
- **Project number / ID:** mixpeek-inference-463103
- **Website:** https://amux.io
- **What the client does:** amux is an open-source tool that runs and coordinates AI coding agents. It uploads short product demo videos to amux's own YouTube channel (@amux_io) and reads each video's views and engagement at 24 hours, 72 hours and 7 days to decide which demos to make next.
- **Channels it acts on:** only @amux_io, owned by hello@amux.io in the amux.io Google Workspace. It does not act for other users or channels.
- **Authorization:** a service account with domain-wide delegation in the amux.io Workspace, impersonating hello@amux.io. No end-user OAuth flow.
- **Scopes:** youtube.upload, youtube.readonly, yt-analytics.readonly.
- **Data stored:** the video id, title, caption and metric snapshots (views, likes, comments, shares, watch minutes) for amux's own videos, in amux's local database. No other channel's data and no viewer personal data.
- **Expected volume:** about one upload a day, plus a few hundred metric reads a day. The default 10,000-unit daily quota covers this (an upload costs 1,600 units).
- **Why the audit:** uploads from unaudited projects are locked private. amux needs its uploads to be public.
