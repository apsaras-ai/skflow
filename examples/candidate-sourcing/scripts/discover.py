"""A deterministic local discovery fixture; replace with trusted data collection."""

import json
import sys


def main():
    state = json.load(sys.stdin)
    candidates = state["input"].get("candidates", [])
    if not isinstance(candidates, list):
        raise ValueError("input.candidates must be an array")
    json.dump(candidates, sys.stdout, ensure_ascii=False)


if __name__ == "__main__":
    main()
