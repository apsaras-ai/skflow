#!/usr/bin/env python3
"""Run the shipped example through the real CLI using only Python's stdlib.

Usage: python3 tests/acceptance.py --binary target/debug/skflow
The suite uses a temporary copy and does not create a run in the checkout.
"""

import argparse
import html
import json
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/skflow")
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        parser.error(f"CLI binary not found: {binary}; run cargo build first")
    checkout = Path(__file__).resolve().parents[1]

    with tempfile.TemporaryDirectory(prefix="skflow acceptance ") as directory:
        root = Path(directory)
        example = root / "candidate sourcing"
        shutil.copytree(checkout / "examples" / "candidate-sourcing", example)
        database = root / "run with spaces" / "state.db"

        def cli(*arguments, code):
            result = subprocess.run(
                [str(binary), "--db", str(database), *arguments],
                cwd=root,
                text=True,
                capture_output=True,
                timeout=30,
            )
            assert result.returncode == code, (
                f"{arguments}: expected exit {code}, got {result.returncode}\n"
                f"stdout: {result.stdout}\nstderr: {result.stderr}"
            )
            try:
                value = json.loads(result.stdout)
            except json.JSONDecodeError as error:
                raise AssertionError(f"CLI must emit one JSON value: {result.stdout}") from error
            assert value["status"] == {
                0: "completed", 10: "waiting", 11: "rejected", 20: "blocked", 1: "error"
            }[code], value
            return value

        input_path = example / "input.json"
        initial = json.loads(input_path.read_text(encoding="utf-8"))
        injected_name = "林舟（虚构）<script>alert('x')</script>"
        initial["candidates"][0]["name"] = injected_name
        input_path.write_text(json.dumps(initial, ensure_ascii=False), encoding="utf-8")

        started = cli(
            "start", str(example / "workflow.yaml"), "--input", str(input_path), code=10
        )
        tasks = started["pending_tasks"]
        assert len(tasks) == 2, tasks
        keys = [item["key"] for task in tasks for item in task["context"]["items"]]
        assert sorted(keys) == ["c-001", "c-002", "c-003"], keys
        assert sorted(len(task["context"]["items"]) for task in tasks) == [1, 2]
        for task in tasks:
            assert task["instruction"]
            for item in task["context"]["items"]:
                context = item["context"]
                assert set(context) == {"candidate", "enriched", "target"}, context
                assert context["candidate"]["id"] == item["key"]
                assert context["enriched"]["portfolio_verified"] is False
            context_path = Path(task["context_file"])
            schema_path = Path(task["schema_file"])
            assert json.loads(context_path.read_text()) == task["context"]
            assert json.loads(schema_path.read_text()) == task["output_schema"]
            context_path.unlink()
            schema_path.unlink()

        snapshot = cli("status", code=10)
        assert len(snapshot["pending_tasks"]) == 2
        cli("inspect", "/candidates", code=10)
        for task in tasks:
            assert not Path(task["context_file"]).exists()
            assert not Path(task["schema_file"]).exists()
        resumed = cli("next", code=10)
        assert [task["task_id"] for task in resumed["pending_tasks"]] == [
            task["task_id"] for task in tasks
        ]
        for task in resumed["pending_tasks"]:
            assert Path(task["context_file"]).is_file()
            assert Path(task["schema_file"]).is_file()
        status = cli("status", code=10)
        assert len(status["pending_tasks"]) == 2

        before = cli("inspect", "", code=10)["value"]
        assert before["input"] == initial
        assert len(before["candidates"]) == 3
        assert set(before["enriched"]) == set(keys)

        def answer_for(task):
            return {
                item["key"]: {
                    "recommendation": "possible",
                    "score": 3,
                    "reason": (
                        "候选人的本地材料与岗位要求存在部分交集，但当前证据全部来自演示输入；"
                        "项目规模、实际职责、材料真实性以及协作表现需要在后续核实，不能仅凭链接作出判断。"
                    ),
                    "evidence": [
                        "已读取候选人在本地输入中声明的技能与项目简介。",
                        "补证步骤明确说明未进行任何外部网络验证。",
                    ],
                }
                for item in task["context"]["items"]
            }

        answer_path = root / "answer.json"

        def submit(task, answer, code):
            answer_path.write_text(json.dumps(answer, ensure_ascii=False), encoding="utf-8")
            return cli("submit", task["task_id"], "--file", str(answer_path), code=code)

        good_first = answer_for(tasks[0])
        bad_key = {**good_first, "unassigned-entity": next(iter(good_first.values()))}
        assert submit(tasks[0], bad_key, 11)["errors"]
        assert cli("inspect", "", code=10)["value"] == before
        missing_key = dict(good_first)
        missing_key.pop(next(iter(missing_key)))
        assert submit(tasks[0], missing_key, 11)["errors"]
        other_task_key = tasks[1]["context"]["items"][0]["key"]
        other_batch = {**good_first, other_task_key: next(iter(good_first.values()))}
        assert submit(tasks[0], other_batch, 11)["errors"]
        assert cli("inspect", "", code=10)["value"] == before
        short_reason = answer_for(tasks[0])
        next(iter(short_reason.values()))["reason"] = "太短"
        assert submit(tasks[0], short_reason, 11)["errors"]
        assert cli("inspect", "", code=10)["value"] == before

        first = submit(tasks[0], good_first, 10)
        assert len(first["pending_tasks"]) == 1
        assert len(submit(tasks[0], good_first, 10)["pending_tasks"]) == 1
        submit(tasks[1], answer_for(tasks[1]), 0)
        completed = cli("inspect", "", code=0)["value"]
        assert completed["raw_data"] == before["raw_data"]
        assert completed["candidates"] == before["candidates"]
        assert completed["enriched"] == before["enriched"]
        assert set(completed["reviews"]) == set(keys)
        report = Path(completed["report"]["path"])
        assert report.is_file()
        document = report.read_text(encoding="utf-8")
        assert html.escape(injected_name, quote=True) in document
        assert "<script>" not in document
        assert completed["report"]["candidate_count"] == 3

        submit(tasks[0], good_first, 0)
        changed_answer = answer_for(tasks[0])
        next(iter(changed_answer.values()))["score"] = 4
        submit(tasks[0], changed_answer, 11)
        cli("next", code=0)
        assert cli("inspect", "", code=0)["value"] == completed

        rewound = cli("rewind", "review", code=10)
        assert len(rewound["pending_tasks"]) == 2
        new_tasks = rewound["pending_tasks"]
        assert {task["task_id"] for task in new_tasks}.isdisjoint(
            {task["task_id"] for task in tasks}
        )
        rewound_state = cli("inspect", "", code=10)["value"]
        assert rewound_state["candidates"] == before["candidates"]
        assert rewound_state["enriched"] == before["enriched"]
        assert not rewound_state.get("reviews")
        assert "report" not in rewound_state
        submit(tasks[0], good_first, 11)
        submit(new_tasks[0], answer_for(new_tasks[0]), 10)
        submit(new_tasks[1], answer_for(new_tasks[1]), 0)

        def case_files(name, workflow, schema):
            case = root / name
            case.mkdir()
            (case / "workflow.yaml").write_text(workflow, encoding="utf-8")
            (case / "input.json").write_text("{}", encoding="utf-8")
            (case / "schema.json").write_text(json.dumps(schema), encoding="utf-8")
            return case

        gate_schema = {
            "type": "object", "additionalProperties": False,
            "properties": {"pass": {"type": "boolean"}}, "required": ["pass"],
        }
        gate = case_files("bounded-check", """version: 1
name: bounded-check
steps:
  assess:
    agent:
      instruction: Decide whether the gate should pass.
      schema: schema.json
    output: /answer
  derive:
    run: python3 derive.py
    output: /derived
  gate:
    check:
      - expr: "answer.pass == `true`"
        message: Approval required
    on_fail: {goto: assess, max_visits: 2}
""", gate_schema)
        (gate / "derive.py").write_text(
            "import json,sys\ns=json.load(sys.stdin)\n"
            "print(json.dumps({'copied':s['answer']['pass']}))\n", encoding="utf-8"
        )
        database = root / "bounded-check.db"
        first_gate = cli(
            "start", str(gate / "workflow.yaml"), "--input", str(gate / "input.json"), code=10
        )["pending_tasks"][0]
        second_gate = submit(first_gate, {"pass": False}, 10)["pending_tasks"][0]
        assert first_gate["task_id"] != second_gate["task_id"]
        loop_state = cli("inspect", "", code=10)["value"]
        assert "answer" not in loop_state and "derived" not in loop_state
        submit(first_gate, {"pass": True}, 11)
        assert submit(second_gate, {"pass": False}, 20)["errors"]
        cli("next", code=20)
        cli("status", code=20)
        assert cli("inspect", "/derived", code=20)["value"] == {"copied": False}
        reset_gate = cli("rewind", "assess", code=10)["pending_tasks"][0]
        assert reset_gate["task_id"] not in {first_gate["task_id"], second_gate["task_id"]}
        submit(second_gate, {"pass": True}, 11)
        submit(reset_gate, {"pass": True}, 0)
        assert cli("inspect", "/derived", code=0)["value"] == {"copied": True}

        workflow = """version: 1
name: explicit-workflow-adoption
steps:
  seed:
    run: python3 seed.py
    output: /seed
  assess:
    agent:
      instruction: Review the seed value.
      context: /seed
      schema: schema.json
    output: /answer
"""
        adoption = case_files("workflow-adoption", workflow, gate_schema)
        (adoption / "seed.py").write_text("print(1)\n", encoding="utf-8")
        (adoption / "seed2.py").write_text("print(2)\n", encoding="utf-8")
        database = root / "workflow-adoption.db"
        original_task = cli(
            "start", str(adoption / "workflow.yaml"),
            "--input", str(adoption / "input.json"), code=10
        )["pending_tasks"][0]
        assert original_task["context"] == 1

        new_schema = {
            **gate_schema,
            "properties": {**gate_schema["properties"], "note": {"type": "string", "minLength": 4}},
            "required": ["pass", "note"],
        }
        (adoption / "schema.json").write_text(json.dumps(new_schema), encoding="utf-8")
        assert cli("next", code=1)["errors"]
        cli("status", code=10)
        assert cli("inspect", "/seed", code=10)["value"] == 1
        submit(original_task, {"pass": True}, 1)
        schema_task = cli("rewind", "assess", "--accept-workflow", code=10)["pending_tasks"][0]
        assert schema_task["task_id"] != original_task["task_id"]
        submit(original_task, {"pass": True, "note": "verified"}, 11)
        submit(schema_task, {"pass": True}, 11)

        changed_workflow = workflow.replace("Review the seed value.", "Review the seed and explain.")
        (adoption / "workflow.yaml").write_text(changed_workflow, encoding="utf-8")
        cli("next", code=1)
        adopted_task = cli("rewind", "assess", "--accept-workflow", code=10)["pending_tasks"][0]
        assert adopted_task["instruction"] == "Review the seed and explain."
        submit(schema_task, {"pass": True, "note": "verified"}, 11)
        submit(adopted_task, {"pass": True, "note": "verified"}, 0)

        renamed_output = changed_workflow.replace("output: /answer", "output: /decision")
        (adoption / "workflow.yaml").write_text(renamed_output, encoding="utf-8")
        renamed_task = cli("rewind", "assess", "--accept-workflow", code=10)["pending_tasks"][0]
        renamed_state = cli("inspect", "", code=10)["value"]
        assert "answer" not in renamed_state and "decision" not in renamed_state
        submit(renamed_task, {"pass": True, "note": "verified"}, 0)
        assert cli("inspect", "/decision", code=0)["value"]["pass"] is True

        changed_prefix = renamed_output.replace("python3 seed.py", "python3 seed2.py")
        (adoption / "workflow.yaml").write_text(changed_prefix, encoding="utf-8")
        assert cli("rewind", "assess", "--accept-workflow", code=1)["errors"]
        assert cli("inspect", "/seed", code=0)["value"] == 1
        rerun_task = cli("rewind", "seed", "--accept-workflow", code=10)["pending_tasks"][0]
        assert rerun_task["context"] == 2
        submit(rerun_task, {"pass": True, "note": "verified"}, 0)

        keyed = case_files("key-normalization", """version: 1
name: key-normalization
steps:
  review:
    foreach: {items: /input/items, key: id, batch: 2}
    agent:
      instruction: Return the supplied integer value for each assigned key.
      context:
        item: "{item}"
        value: "/input/lookup/{key}"
      schema: schema.json
    output: /reviews
""", {"type": "integer"})
        keyed_input = {
            "items": [{"id": "one/~"}, {"id": 7}, {"id": "last"}],
            "lookup": {"one/~": 10, "7": 20, "last": 30},
        }
        (keyed / "input.json").write_text(json.dumps(keyed_input), encoding="utf-8")
        database = root / "escaped-keys.db"
        keyed_tasks = cli(
            "start", str(keyed / "workflow.yaml"), "--input", str(keyed / "input.json"), code=10
        )["pending_tasks"]
        for index, task in enumerate(keyed_tasks):
            answer = {item["key"]: item["context"]["value"] for item in task["context"]["items"]}
            submit(task, answer, 0 if index == len(keyed_tasks) - 1 else 10)
        assert cli("inspect", "/reviews/one~1~0", code=0)["value"] == 10
        assert cli("inspect", "/reviews/7", code=0)["value"] == 20

        for name, items in [
            ("blank", [{"id": "   "}]),
            ("collision", [{"id": "1"}, {"id": 1}]),
            ("missing", [{"name": "no id"}]),
        ]:
            database = root / f"invalid-keys-{name}.db"
            (keyed / "input.json").write_text(
                json.dumps({"items": items, "lookup": {"   ": 1, "1": 1}}), encoding="utf-8"
            )
            invalid = cli(
                "start", str(keyed / "workflow.yaml"), "--input", str(keyed / "input.json"), code=1
            )
            assert invalid["errors"] and invalid["pending_tasks"] == []
        database = root / "empty-foreach.db"
        (keyed / "input.json").write_text('{"items": []}', encoding="utf-8")
        cli("start", str(keyed / "workflow.yaml"), "--input", str(keyed / "input.json"), code=0)
        assert cli("inspect", "/reviews", code=0)["value"] == {}

    print(
        "PASS: shipped example, task recovery, schema/key fences, immutable facts, "
        "report, idempotency, rewind, bounded checks, workflow/schema adoption"
    )


if __name__ == "__main__":
    main()
