#!/usr/bin/env python3
"""GitHub: stars and forks of the open-source repo, daily (acquisition line of the report)."""
import _common as c

c.run("github", lambda: {k: v for k, v in c.fetch("https://api.github.com/repos/mixpeek/amux").items()
                          if k in ("stargazers_count", "forks_count", "open_issues_count")})
