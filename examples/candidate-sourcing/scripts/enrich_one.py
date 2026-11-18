"""Derive objective facts from local input; do not claim network verification."""

import json
import sys


def main():
    payload = json.load(sys.stdin)
    candidate = payload["item"]
    target = payload["state"]["input"]["target"]
    skills = set(candidate.get("skills", []))
    required = target.get("required_skills", [])
    result = {
        "matched_declared_skills": [skill for skill in required if skill in skills],
        "missing_declared_skills": [skill for skill in required if skill not in skills],
        "portfolio_provided": bool(candidate.get("portfolio_url")),
        "portfolio_verified": False,
        "source": "local input fixture; no external verification performed",
    }
    json.dump(result, sys.stdout, ensure_ascii=False)


if __name__ == "__main__":
    main()
