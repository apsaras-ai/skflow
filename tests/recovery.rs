//! Process-level tests exercise locks and durable progress across independent CLIs.

#![cfg(unix)]

use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn command(directory: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skflow"));
    command
        .current_dir(directory)
        .arg("--db")
        .arg(directory.join("state.db"))
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn decode(output: Output) -> (i32, Value) {
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "CLI must emit exactly one JSON value: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap(), value)
}

fn run(directory: &Path, arguments: &[&str]) -> (i32, Value) {
    decode(command(directory, arguments).output().unwrap())
}

fn put(directory: &Path, name: &str, content: &str) {
    fs::write(directory.join(name), content).unwrap();
}

#[test]
fn concurrent_submissions_preserve_every_result_and_advance_once() {
    let directory = tempdir().unwrap();
    let directory = directory.path();
    put(
        directory,
        "workflow.yaml",
        r#"version: 1
name: concurrent-submit
steps:
  review:
    foreach: {items: /input/items, key: id}
    agent:
      instruction: Write a grounded reason.
      context: {candidate: "{item}"}
      schema: review.json
    output: /reviews
  render:
    run: python3 render.py
    output: /report
"#,
    );
    put(
        directory,
        "review.json",
        r#"{"type":"object","properties":{"reason":{"type":"string","minLength":3}},"required":["reason"],"additionalProperties":false}"#,
    );
    put(
        directory,
        "render.py",
        "import json, sys\nfrom pathlib import Path\nstate = json.load(sys.stdin)\nwith open('renders', 'a') as f: f.write('x')\nprint(json.dumps({'review_count': len(state['reviews'])}))\n",
    );
    let input = json!({"items": (0..8).map(|index| json!({"id": format!("item{index}")})).collect::<Vec<_>>()});
    put(directory, "input.json", &input.to_string());
    let (code, response) = run(
        directory,
        &["start", "workflow.yaml", "--input", "input.json"],
    );
    assert_eq!(code, 10, "{response}");
    let tasks = response["pending_tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 8);
    let mut children = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        let key = task["keys"][0].as_str().unwrap();
        let answer = json!({key: {"reason": format!("Grounded result for {key}")}});
        let filename = format!("answer-{index}.json");
        put(directory, &filename, &answer.to_string());
        children.push(
            command(
                directory,
                &[
                    "submit",
                    task["task_id"].as_str().unwrap(),
                    "--file",
                    &filename,
                ],
            )
            .spawn()
            .unwrap(),
        );
    }
    for child in children {
        let (code, result) = decode(child.wait_with_output().unwrap());
        assert!([0, 10].contains(&code), "{result}");
    }
    let (code, status) = run(directory, &["status"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["status"], "completed");
    assert!(status["pending_tasks"].as_array().unwrap().is_empty());
    assert_eq!(fs::read(directory.join("renders")).unwrap(), b"x");
    let (code, inspected) = run(directory, &["inspect", "/reviews"]);
    assert_eq!(code, 0, "{inspected}");
    let reviews = inspected["value"].as_object().unwrap();
    assert_eq!(reviews.len(), 8);
    for index in 0..8 {
        assert_eq!(
            reviews[&format!("item{index}")]["reason"],
            format!("Grounded result for item{index}")
        );
    }
}

#[test]
fn concurrent_next_does_not_duplicate_deterministic_work() {
    let directory = tempdir().unwrap();
    let directory = directory.path();
    put(
        directory,
        "workflow.yaml",
        "version: 1\nname: concurrent-next\nsteps:\n  work:\n    run: python3 work.py\n    output: /result\n",
    );
    put(directory, "input.json", "{}");
    put(
        directory,
        "work.py",
        "import json, sys, time\nfrom pathlib import Path\nif not Path('allow').exists(): sys.exit(5)\nwith open('executions', 'a') as f: f.write('x')\ntime.sleep(0.15)\nprint(json.dumps({'ok': True}))\n",
    );
    let (code, response) = run(
        directory,
        &["start", "workflow.yaml", "--input", "input.json"],
    );
    assert_eq!(code, 1, "{response}");
    put(directory, "allow", "yes");
    let children: Vec<_> = (0..6)
        .map(|_| command(directory, &["next"]).spawn().unwrap())
        .collect();
    for child in children {
        let (code, response) = decode(child.wait_with_output().unwrap());
        assert_eq!(code, 0, "{response}");
        assert_eq!(response["status"], "completed");
    }
    assert_eq!(fs::read(directory.join("executions")).unwrap(), b"x");
}

struct InterruptedProcess {
    child: Child,
    group_file: Option<PathBuf>,
}

impl InterruptedProcess {
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SIGKILL deliberately skips the CLI's cleanup handlers. Clean up the
        // trusted test script separately after proving the parent was killed.
        let Some(group_file) = self.group_file.take() else {
            return;
        };
        if let Ok(text) = fs::read_to_string(group_file) {
            if let Ok(group) = text.trim().parse::<i32>() {
                if group > 1 {
                    // SAFETY: this file was written by our own child script,
                    // launched by skflow in an isolated process group.
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                }
            }
        }
    }
}

impl Drop for InterruptedProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn killed_foreach_resumes_only_items_without_a_committed_result() {
    let directory = tempdir().unwrap();
    let directory = directory.path();
    put(
        directory,
        "workflow.yaml",
        "version: 1\nname: killed-foreach\nsteps:\n  enrich:\n    foreach: {items: /input/items, key: id, concurrency: 1}\n    run: python3 enrich.py\n    output: /results\n    timeout: 10s\n",
    );
    put(
        directory,
        "input.json",
        r#"{"items":[{"id":"first"},{"id":"second"},{"id":"third"}]}"#,
    );
    put(
        directory,
        "enrich.py",
        "import json, os, sys, time\nfrom pathlib import Path\nitem = json.load(sys.stdin)\nkey = item['key']\nwith open('attempts', 'a') as f: f.write(key + '\\n')\nif key == 'second' and not Path('release').exists():\n Path('script-group').write_text(str(os.getpgrp()))\n Path('second-started').touch()\n time.sleep(8)\nprint(json.dumps({'done': key}))\n",
    );
    let mut process = InterruptedProcess {
        child: command(
            directory,
            &["start", "workflow.yaml", "--input", "input.json"],
        )
        .spawn()
        .unwrap(),
        group_file: Some(directory.join("script-group")),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut committed = false;
    while Instant::now() < deadline {
        if directory.join("second-started").exists() {
            // The persisted JSON is the source of truth. Observing the second
            // script alone is insufficient: a worker may start before the main
            // thread has committed the first worker's result.
            if let Ok(store) = skflow::store::Store::open(&directory.join("state.db"), true) {
                if let Ok(document) = store.load::<Value>() {
                    if document["state"]["results"]["first"]["done"] == "first" {
                        committed = true;
                        break;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    process.stop();
    assert!(
        committed,
        "the first item was not durably committed before interruption"
    );
    put(directory, "release", "yes");
    let (code, response) = run(directory, &["next"]);
    assert_eq!(code, 0, "{response}");
    let attempts = fs::read_to_string(directory.join("attempts")).unwrap();
    assert_eq!(attempts.lines().filter(|key| *key == "first").count(), 1);
    assert_eq!(attempts.lines().filter(|key| *key == "second").count(), 2);
    assert_eq!(attempts.lines().filter(|key| *key == "third").count(), 1);
    let (code, inspected) = run(directory, &["inspect", "/results"]);
    assert_eq!(code, 0, "{inspected}");
    assert_eq!(inspected["value"].as_object().unwrap().len(), 3);
}

#[test]
fn batch_run_fences_keys_preserves_success_and_keeps_the_tail_batch_protocol() {
    let directory = tempdir().unwrap();
    let directory = directory.path();
    put(
        directory,
        "workflow.yaml",
        "version: 1\nname: batch-recovery\nsteps:\n  enrich:\n    foreach: {items: /input/items, key: id, batch: 2, concurrency: 1}\n    run: python3 enrich.py\n    output: /results\n",
    );
    put(
        directory,
        "input.json",
        r#"{"items":[{"id":"a"},{"id":"b"},{"id":"c"},{"id":"d"},{"id":"e"}]}"#,
    );
    put(
        directory,
        "enrich.py",
        r#"import json, sys
from pathlib import Path
payload = json.load(sys.stdin)
assert 'items' in payload and 'item' not in payload
keys = [entry['key'] for entry in payload['items']]
with open('attempts', 'a') as stream:
    stream.write(','.join(keys) + '\n')
results = {key: {'done': key} for key in keys}
if keys == ['c', 'd']:
    mode = Path('mode').read_text()
    if mode == 'missing':
        results.pop('d')
    elif mode == 'extra':
        results['foreign'] = {'done': 'foreign'}
print(json.dumps(results))
"#,
    );
    put(directory, "mode", "missing");
    let (code, response) = run(
        directory,
        &["start", "workflow.yaml", "--input", "input.json"],
    );
    assert_eq!(code, 1, "{response}");
    for mode in ["extra", "valid"] {
        let (code, inspected) = run(directory, &["inspect", "/results"]);
        assert_eq!(code, 1, "{inspected}");
        assert_eq!(
            inspected["value"],
            json!({"a": {"done": "a"}, "b": {"done": "b"}}),
            "a rejected batch must not partially write its results"
        );
        put(directory, "mode", mode);
        let (code, response) = run(directory, &["next"]);
        assert_eq!(code, if mode == "valid" { 0 } else { 1 }, "{response}");
    }
    let (_, inspected) = run(directory, &["inspect", "/results"]);
    assert_eq!(inspected["value"].as_object().unwrap().len(), 5);
    assert_eq!(inspected["value"]["e"], json!({"done": "e"}));
    let attempts = fs::read_to_string(directory.join("attempts")).unwrap();
    assert_eq!(
        attempts.lines().collect::<Vec<_>>(),
        ["a,b", "c,d", "c,d", "c,d", "e"]
    );
}

#[test]
fn concurrent_retries_of_the_same_final_task_are_idempotent() {
    let directory = tempdir().unwrap();
    let directory = directory.path();
    put(
        directory,
        "workflow.yaml",
        r#"version: 1
name: duplicate-final-submit
steps:
  review:
    agent:
      instruction: Provide a reason.
      schema: review.json
    output: /review
  render:
    run: python3 render.py
    output: /report
"#,
    );
    put(directory, "input.json", "{}");
    put(
        directory,
        "review.json",
        r#"{"type":"object","properties":{"reason":{"type":"string"}},"required":["reason"],"additionalProperties":false}"#,
    );
    put(
        directory,
        "answer.json",
        r#"{"reason":"same accepted answer"}"#,
    );
    put(
        directory,
        "render.py",
        "import time\nwith open('renders', 'a') as stream: stream.write('x')\ntime.sleep(0.1)\nprint('{}')\n",
    );
    let (code, response) = run(
        directory,
        &["start", "workflow.yaml", "--input", "input.json"],
    );
    assert_eq!(code, 10, "{response}");
    let task_id = response["pending_tasks"][0]["task_id"].as_str().unwrap();
    let children: Vec<_> = (0..2)
        .map(|_| {
            command(directory, &["submit", task_id, "--file", "answer.json"])
                .spawn()
                .unwrap()
        })
        .collect();
    let mut acknowledgements = 0;
    for child in children {
        let (code, response) = decode(child.wait_with_output().unwrap());
        assert_eq!(code, 0, "{response}");
        assert_eq!(response["status"], "completed");
        if response["idempotent"] == true {
            acknowledgements += 1;
        }
    }
    assert_eq!(acknowledgements, 1);
    assert_eq!(fs::read(directory.join("renders")).unwrap(), b"x");
}
