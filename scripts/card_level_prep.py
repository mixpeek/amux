"""Provisional, offline difficulty labels. Never used to authorize dispatch."""
import argparse
import json
import re
from collections.abc import Mapping
from pathlib import Path


def classify(card: Mapping[str, object]) -> dict:
    """Return a conservative shadow label; source text cannot grant authority."""
    if not isinstance(card, Mapping):
        raise ValueError("card must be an object")
    identifier = card.get("id")
    if not isinstance(identifier, str) or not identifier.strip():
        raise ValueError("card requires a nonempty string id")
    title = card.get("title")
    if not isinstance(title, str) or not title.strip():
        raise ValueError("card requires a nonempty string title")
    # Include all supplied fields, including nested hold metadata. Even negated
    # safety language raises the floor: this shadow tool cannot clear a hold.
    text = json.dumps(dict(card), ensure_ascii=True, sort_keys=True).lower()
    scope = "\n".join(str(card.get(key) or "") for key in ("title", "desc", "description"))
    verify = re.search(r"(?im)\bverify:[ \t]*([^\r\n]+)", scope)
    has_verify = bool(verify and verify.group(1).strip())
    if re.search(r"\b(credentials?|secrets?|sudo|root|trading|orders?|sealed|reseal)\b|production[ -]deploy|protection hold|security hold|owner hold|safety hold", text):
        level, reason, uncertain = 3, "sensitive text or hold requires frontier safety review", True
    elif re.search(r"\b(integration|architect(?:ure)?|review|scheduler|atomic|migration|concurrency)\b", text):
        level, reason, uncertain = 2, "integration or review requires stronger lane", False
    elif (card.get("type") == "code" and has_verify
          and re.search(r"[a-z0-9_./-]+\.(?:py|rs|go|ts|tsx|js|json|md)\b", text)
          and re.search(r"\b(one change|single file|single-file|bounded)\b", text)):
        level, reason, uncertain = 1, "bounded code names a file and verification command", False
    elif (card.get("type") in ("chore", "doc")
          and re.fullmatch(r"(?:list|inventory) (?:files|filenames|directories)|(?:format|ascii check|duplicate check) [a-z0-9_./-]+", title.lower())
          and not re.search(r"\b(code|implement|build|change|fix|deploy|service|timer|investigate|discrepanc(?:y|ies)|decide|evaluate|unclear)\b", text)):
        level, reason, uncertain = 0, "explicit mechanical non-code chore", False
    else:
        level, reason, uncertain = 2, "unclear scope rounded up; local-model intake still needed", True
    return {"id": identifier, "level": level, "reasons": [reason], "needs_model": uncertain}


def prepare(payload: object) -> dict:
    if not isinstance(payload, dict) or not isinstance(payload.get("cards"), list):
        raise ValueError("input requires an object with a cards list")
    rows = [classify(card) for card in payload["cards"]]
    ids = [row["id"] for row in rows]
    if len(ids) != len(set(ids)):
        raise ValueError("duplicate card id")
    return {"schema_version": 1, "provisional": True, "cards": rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.input.resolve() == args.output.resolve():
        parser.error("input and output must differ")
    try:
        result = prepare(json.loads(args.input.read_text()))
    except (ValueError, TypeError) as exc:
        parser.error(str(exc))
    args.output.write_text(json.dumps(result, indent=2, ensure_ascii=True) + "\n")


if __name__ == "__main__":
    main()
