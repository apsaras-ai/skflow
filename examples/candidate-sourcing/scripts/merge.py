"""Normalize duplicate IDs while preserving the first observed candidate."""

import json
import sys


def main():
    state = json.load(sys.stdin)
    candidates = {}
    for item in state["raw_data"]:
        key = item.get("id")
        if isinstance(key, bool) or not isinstance(key, (str, int)):
            raise ValueError("each candidate must have a string or integer id")
        key = str(key)
        if not key.strip():
            raise ValueError("candidate id must not be blank")
        candidates.setdefault(key, {**item, "id": key})
    json.dump(list(candidates.values()), sys.stdout, ensure_ascii=False)


if __name__ == "__main__":
    main()
