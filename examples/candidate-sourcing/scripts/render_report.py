"""Render a deterministic report using only the Python standard library."""

import html
import json
from pathlib import Path
import sys


def escape(value):
    return html.escape(str(value), quote=True)


def main():
    state = json.load(sys.stdin)
    candidates = state["candidates"]
    reviews = state["reviews"]
    cards = []
    for candidate in candidates:
        review = reviews[str(candidate["id"])]
        evidence = "".join(f"<li>{escape(item)}</li>" for item in review["evidence"])
        cards.append(
            f"<article><h2>{escape(candidate['name'])}</h2>"
            f"<p class='meta'>{escape(review['recommendation'])} · "
            f"{escape(review['score'])}/5</p>"
            f"<p>{escape(review['reason'])}</p><ul>{evidence}</ul></article>"
        )
    document = """<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Candidate sourcing · skflow</title>
<style>
body{margin:0;background:#f4f6fa;color:#192b40;font:16px/1.7 system-ui,sans-serif}
main{max-width:850px;margin:48px auto;padding:0 24px}
h1{line-height:1.2}h2{font-size:21px;margin:0}
article{background:#fff;border:1px solid #dbe2ed;border-radius:12px;padding:24px;margin:20px 0}
.meta{color:#426493;font-weight:600}.note{color:#617085;font-size:14px}
</style>
</head><body><main>
<h1>候选人评估报告</h1>
<p class="note">虚构示例，仅展示 skflow 工作流；材料未经过外部验证。</p>
""" + "\n".join(cards) + "\n</main></body></html>\n"
    path = Path("report.html").resolve()
    path.write_text(document, encoding="utf-8")
    json.dump({"path": str(path), "candidate_count": len(candidates)}, sys.stdout)


if __name__ == "__main__":
    main()
