//! Durable progress and task ownership. All writes run under the database's OS lock.
use crate::{
    config::{self, LoadedWorkflow, Step},
    data, runner,
    store::{self, RunLock, Store},
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug)]
pub enum Command {
    Start { workflow: PathBuf, input: PathBuf },
    Next,
    Submit { task_id: String, file: PathBuf },
    Status,
    Inspect { path: String },
    Rewind { step: String, accept_workflow: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Item {
    key: String,
    item: Value,
    done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Progress {
    name: String,
    generation: u64,
    visits: u32,
    phase: String,
    initialized: bool,
    items: Vec<Item>,
}

impl Progress {
    fn new(name: String) -> Self {
        Self {
            name,
            generation: 0,
            visits: 0,
            phase: "pending".into(),
            initialized: false,
            items: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Task {
    id: String,
    step_index: usize,
    step: String,
    generation: u64,
    keys: Vec<String>,
    instruction: String,
    context: Value,
    output_schema: Value,
    item_schema: Value,
    foreach: bool,
    phase: String,
    accepted: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Run {
    id: String,
    workflow_path: PathBuf,
    spec: LoadedWorkflow,
    state: Value,
    cursor: usize,
    status: String,
    progress: Vec<Progress>,
    tasks: Vec<Task>,
    errors: Vec<Value>,
}

pub fn exit_code(response: &Value) -> i32 {
    match response["status"].as_str() {
        Some("completed") => 0,
        Some("waiting") => 10,
        Some("rejected") => 11,
        Some("blocked") => 20,
        _ => 1,
    }
}

/// A read operation uses a committed SQLite snapshot and never takes the long
/// advancement lock. Submissions and advances reload state *after* acquiring it.
pub fn execute(db: &Path, command: Command) -> Result<Value> {
    let is_start = matches!(&command, Command::Start { .. });
    let db = store::absolute_db(db, is_start)?;
    if matches!(&command, Command::Status | Command::Inspect { .. }) {
        let store = Store::open(&db, true)?;
        let run: Run = store.load()?;
        let mut response = describe_with_artifacts(&run, &db, false)?;
        if let Command::Inspect { path } = command {
            response["path"] = json!(path);
            response["value"] = data::get(&run.state, &path)?;
        }
        return Ok(response);
    }
    let _lock = RunLock::acquire(&db)?;
    if let Command::Start { workflow, input } = command {
        let workflow_path = workflow.canonicalize().context("workflow does not exist")?;
        let spec = config::load(&workflow_path)?;
        let input: Value = serde_json::from_slice(&fs::read(&input).context("read input JSON")?)
            .context("invalid input JSON")?;
        let progress = spec
            .workflow
            .steps
            .keys()
            .cloned()
            .map(Progress::new)
            .collect();
        let mut run = Run {
            id: Uuid::new_v4().to_string(),
            workflow_path,
            spec,
            state: json!({"input": input}),
            cursor: 0,
            status: "waiting".into(),
            progress,
            tasks: vec![],
            errors: vec![],
        };
        let mut store = Store::create(&db, &run)?;
        drive(&mut store, &mut run)?;
        return describe(&run, &db);
    }

    let mut store = Store::open(&db, false)?;
    let mut run: Run = store.load()?;
    match command {
        Command::Next => {
            ensure_unchanged(&run)?;
            drive(&mut store, &mut run)?;
            describe(&run, &db)
        }
        Command::Submit { task_id, file } => {
            ensure_unchanged(&run)?;
            let bytes = fs::read(file).context("read answer file")?;
            let answer = match serde_json::from_slice(&bytes) {
                Ok(answer) => answer,
                Err(error) => {
                    return rejected(
                        &run,
                        &db,
                        vec![json!({"code":"invalid_json", "message":error.to_string()})],
                    )
                }
            };
            submit(&mut store, &mut run, &db, &task_id, answer)
        }
        Command::Rewind {
            step,
            accept_workflow,
        } => {
            rewind(&mut run, &step, accept_workflow)?;
            store.save(&run)?;
            drive(&mut store, &mut run)?;
            describe(&run, &db)
        }
        _ => unreachable!("handled before database open"),
    }
}

fn ensure_unchanged(run: &Run) -> Result<()> {
    let latest = config::load(&run.workflow_path)
        .context("cannot verify workflow; status/inspect remain available")?;
    ensure!(latest.digest == run.spec.digest,
        "workflow or schema changed; restore the original files, or run rewind <earliest_changed_step> --accept-workflow to explicitly adopt the changes");
    Ok(())
}

fn drive(store: &mut Store, run: &mut Run) -> Result<()> {
    if run.status == "blocked" || run.status == "completed" {
        return Ok(());
    }
    run.status = "waiting".into();
    run.errors.clear();
    if let Err(error) = advance(store, run) {
        run.status = "error".into();
        run.errors = vec![
            json!({"code":"execution_error", "step":run.progress.get(run.cursor).map(|p| &p.name), "message":format!("{error:#}")}),
        ];
        store.save(run)?;
    }
    Ok(())
}

fn advance(store: &mut Store, run: &mut Run) -> Result<()> {
    while run.cursor < run.progress.len() {
        let index = run.cursor;
        let (name, step) = run
            .spec
            .workflow
            .steps
            .get_index(index)
            .context("invalid step cursor")?;
        let (name, step) = (name.clone(), step.clone());
        initialize(store, run, &step)?;

        if step.agent.is_some() {
            if step.foreach.is_some() && run.progress[index].items.is_empty() {
                complete(store, run)?;
                continue;
            }
            let generation = run.progress[index].generation;
            if !run
                .tasks
                .iter()
                .any(|t| t.step_index == index && t.generation == generation && t.phase != "stale")
            {
                make_tasks(run, &name, &step)?;
            }
            let pending = run.tasks.iter().any(|t| {
                t.step_index == index && t.generation == generation && t.phase == "pending"
            });
            if pending {
                run.progress[index].phase = "waiting".into();
                run.status = "waiting".into();
                store.save(run)?;
                return Ok(());
            }
            complete(store, run)?;
        } else if step.run.is_some() {
            run_scripts(store, run, &step)?;
            complete(store, run)?;
        } else {
            let mut errors = Vec::new();
            for assertion in step.check.as_ref().expect("validated check step") {
                let expression =
                    jmespath::compile(&assertion.expr).map_err(|e| anyhow!(e.to_string()))?;
                let result = expression.search(&run.state).map_err(|e| {
                    anyhow!("JMESPath evaluation failed for {}: {e}", assertion.expr)
                })?;
                if result.as_boolean() != Some(true) {
                    errors.push(json!({"code":"check_failed", "step":name, "expr":assertion.expr, "message":assertion.message, "actual":result}));
                }
            }
            if errors.is_empty() {
                complete(store, run)?;
                continue;
            }
            if let Some(on_fail) = &step.on_fail {
                let target = run
                    .spec
                    .workflow
                    .steps
                    .get_index_of(&on_fail.goto)
                    .expect("validated goto");
                if run.progress[target].visits < on_fail.max_visits {
                    invalidate(run, target, false)?;
                    store.save(run)?;
                    continue;
                }
                errors.push(json!({"code":"max_visits", "step":on_fail.goto, "visits":run.progress[target].visits, "max_visits":on_fail.max_visits, "message":"automatic retry budget exhausted"}));
            }
            run.status = "blocked".into();
            run.progress[index].phase = "blocked".into();
            run.errors = errors;
            store.save(run)?;
            return Ok(());
        }
    }
    run.status = "completed".into();
    run.errors.clear();
    store.save(run)
}

fn initialize(store: &mut Store, run: &mut Run, step: &Step) -> Result<()> {
    let index = run.cursor;
    if run.progress[index].initialized {
        return Ok(());
    }
    let mut items = vec![];
    if let Some(each) = &step.foreach {
        let array = data::get(&run.state, &each.items)?;
        let array = array
            .as_array()
            .context("foreach.items must point to an array")?;
        let mut seen = BTreeSet::new();
        for item in array {
            let value = item
                .get(&each.key)
                .with_context(|| format!("foreach item is missing key field {}", each.key))?;
            let key = match value {
                Value::String(text) if !text.trim().is_empty() => text.clone(),
                Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
                _ => bail!("foreach key must be a nonempty string or integer: {value}"),
            };
            ensure!(seen.insert(key.clone()), "duplicate foreach key: {key}");
            items.push(Item {
                key,
                item: item.clone(),
                done: false,
            });
        }
        data::set(
            &mut run.state,
            step.output.as_deref().expect("validated output"),
            json!({}),
        )?;
    }
    let progress = &mut run.progress[index];
    progress.visits = progress
        .visits
        .checked_add(1)
        .context("step visit counter overflow")?;
    progress.initialized = true;
    progress.phase = "running".into();
    progress.items = items;
    store.save(run)
}

fn complete(store: &mut Store, run: &mut Run) -> Result<()> {
    run.progress[run.cursor].phase = "completed".into();
    run.cursor += 1;
    store.save(run)
}

fn make_tasks(run: &mut Run, name: &str, step: &Step) -> Result<()> {
    let agent = step.agent.as_ref().expect("agent step");
    let schema = run
        .spec
        .schemas
        .get(name)
        .context("missing frozen schema")?
        .clone();
    let index = run.cursor;
    let generation = run.progress[index].generation;
    let groups: Vec<Vec<Item>> = match &step.foreach {
        Some(each) => run.progress[index]
            .items
            .chunks(each.batch)
            .map(|items| items.to_vec())
            .collect(),
        None => vec![vec![]],
    };
    let mut tasks = vec![];
    for items in groups {
        let keys: Vec<String> = items.iter().map(|i| i.key.clone()).collect();
        let context = if step.foreach.is_some() {
            let contexts: Result<Vec<Value>> = items.iter().map(|item| Ok(json!({
                "key":item.key,
                "context":data::project(&agent.context, &run.state, Some(&item.item), Some(&item.key))?
            }))).collect();
            json!({"items":contexts?})
        } else {
            data::project(&agent.context, &run.state, None, None)?
        };
        let output_schema = if step.foreach.is_some() {
            data::batch_output_schema(&schema, &keys)?
        } else {
            schema.clone()
        };
        tasks.push(Task {
            id: Uuid::new_v4().to_string(),
            step_index: index,
            step: name.into(),
            generation,
            keys,
            instruction: agent.instruction.clone(),
            context,
            output_schema,
            item_schema: schema.clone(),
            foreach: step.foreach.is_some(),
            phase: "pending".into(),
            accepted: None,
        });
    }
    run.tasks.extend(tasks);
    Ok(())
}

fn exact_keys(answer: &Value, keys: &[String]) -> Vec<Value> {
    let Some(object) = answer.as_object() else {
        return vec![
            json!({"code":"key_fence", "message":"foreach answer must be an object keyed by exactly the assigned keys", "expected_keys":keys}),
        ];
    };
    let expected: BTreeSet<&String> = keys.iter().collect();
    let actual: BTreeSet<&String> = object.keys().collect();
    if actual == expected {
        return vec![];
    }
    vec![
        json!({"code":"key_fence", "message":"answer keys must exactly match this task", "missing_keys":expected.difference(&actual).collect::<Vec<_>>(), "unexpected_keys":actual.difference(&expected).collect::<Vec<_>>()}),
    ]
}

fn submit(store: &mut Store, run: &mut Run, db: &Path, id: &str, answer: Value) -> Result<Value> {
    let Some(task_index) = run.tasks.iter().position(|t| t.id == id) else {
        return rejected(
            run,
            db,
            vec![json!({"code":"unknown_task", "message":"task does not belong to this run"})],
        );
    };
    let task = run.tasks[task_index].clone();
    let current_generation = run.progress.get(task.step_index).map(|p| p.generation);
    if task.phase == "stale" || current_generation != Some(task.generation) {
        return rejected(
            run,
            db,
            vec![
                json!({"code":"stale_task", "message":"task was invalidated by rewind or a check retry; run next"}),
            ],
        );
    }
    if task.phase == "submitted" {
        if task.accepted.as_ref() == Some(&answer) {
            // Retry the acknowledgement only. Never re-run side effects here.
            let mut response = describe(run, db)?;
            response["idempotent"] = json!(true);
            return Ok(response);
        }
        return rejected(
            run,
            db,
            vec![
                json!({"code":"already_submitted", "message":"task already accepted a different answer; use rewind to change it"}),
            ],
        );
    }
    if run.cursor != task.step_index {
        return rejected(
            run,
            db,
            vec![json!({"code":"inactive_task", "message":"task is not at the current step"})],
        );
    }
    let mut errors = if task.foreach {
        exact_keys(&answer, &task.keys)
    } else {
        vec![]
    };
    if errors.is_empty() {
        if task.foreach {
            for key in &task.keys {
                for mut error in data::validate(&task.item_schema, &answer[key])? {
                    error["key"] = json!(key);
                    error["code"] = json!("schema_validation");
                    errors.push(error);
                }
            }
        } else {
            errors = data::validate(&task.item_schema, &answer)?;
        }
    }
    if !errors.is_empty() {
        return rejected(run, db, errors);
    }
    let step = &run.spec.workflow.steps[&task.step];
    let output = step.output.as_deref().expect("validated output");
    // Validate into a private tree first, then commit both state and task atomically.
    let mut state = run.state.clone();
    if task.foreach {
        for key in &task.keys {
            data::set(
                &mut state,
                &format!("{output}/{}", data::escape_token(key)),
                answer[key].clone(),
            )?;
        }
    } else {
        data::set(&mut state, output, answer.clone())?;
    }
    run.state = state;
    run.tasks[task_index].phase = "submitted".into();
    run.tasks[task_index].accepted = Some(answer);
    for item in &mut run.progress[task.step_index].items {
        if task.keys.contains(&item.key) {
            item.done = true;
        }
    }
    run.status = "waiting".into();
    run.errors.clear();
    store.save(run)?;
    drive(store, run)?;
    describe(run, db)
}

#[derive(Clone)]
struct Job {
    keys: Vec<String>,
    input: Value,
}

fn run_scripts(store: &mut Store, run: &mut Run, step: &Step) -> Result<()> {
    let command = step.run.as_deref().expect("run step");
    let timeout = step
        .timeout
        .as_deref()
        .map(humantime::parse_duration)
        .transpose()?
        .unwrap_or(Duration::from_secs(300));
    let cwd = run
        .workflow_path
        .parent()
        .context("workflow directory missing")?
        .to_path_buf();
    let output = step.output.as_deref().expect("validated output");
    let Some(each) = &step.foreach else {
        let input = match &step.input {
            Some(template) => data::project(template, &run.state, None, None)?,
            None => run.state.clone(),
        };
        let result = runner::run(command, &cwd, &input, timeout, step.retry)?;
        data::set(&mut run.state, output, result)?;
        // Output and completion are one commit; a crash cannot re-execute an
        // already committed ordinary run step.
        // `advance` immediately calls `complete`, persisting the output and the
        // new cursor in the same transaction.
        return Ok(());
    };
    let pending: Vec<Item> = run.progress[run.cursor]
        .items
        .iter()
        .filter(|i| !i.done)
        .cloned()
        .collect();
    // Keep one input snapshot per invocation and materialize at most one group
    // of concurrent batches, rather than cloning the entire state for all items.
    let snapshot = run.state.clone();
    for group_items in pending.chunks(each.batch * each.concurrency) {
        let mut jobs = vec![];
        for items in group_items.chunks(each.batch) {
            let selected_state = match &step.input {
                Some(template) if each.batch == 1 => data::project(
                    template,
                    &snapshot,
                    Some(&items[0].item),
                    Some(&items[0].key),
                )?,
                Some(template) => data::project(template, &snapshot, None, None)?,
                None => snapshot.clone(),
            };
            let input = if each.batch == 1 {
                json!({"state":selected_state,"item":items[0].item,"key":items[0].key})
            } else {
                json!({"state":selected_state,"items":items.iter().map(|i|json!({"key":i.key,"item":i.item})).collect::<Vec<_>>()})
            };
            jobs.push(Job {
                keys: items.iter().map(|i| i.key.clone()).collect(),
                input,
            });
        }
        let mut failures = vec![];
        std::thread::scope(|scope| -> Result<()> {
            let (send, receive) = mpsc::channel();
            for job in &jobs {
                let send = send.clone();
                let cwd = &cwd;
                scope.spawn(move || {
                    let result = runner::run(command, cwd, &job.input, timeout, step.retry);
                    let _ = send.send((&job.keys, result));
                });
            }
            drop(send);
            for (keys, result) in receive {
                match result.and_then(|answer| {
                    apply_script_batch(run, output, keys, answer, each.batch == 1)
                }) {
                    Ok(()) => store.save(run)?,
                    Err(error) => {
                        failures.push(format!("keys {}: {error:#}", serde_json::to_string(keys)?))
                    }
                }
            }
            Ok(())
        })?;
        if !failures.is_empty() {
            bail!("{}", failures.join("; "));
        }
    }
    Ok(())
}

fn apply_script_batch(
    run: &mut Run,
    output: &str,
    keys: &[String],
    answer: Value,
    single: bool,
) -> Result<()> {
    if !single {
        let errors = exact_keys(&answer, keys);
        ensure!(
            errors.is_empty(),
            "script batch key fence failed: {}",
            json!(errors)
        );
    }
    let mut state = run.state.clone();
    for key in keys {
        let value = if single {
            answer.clone()
        } else {
            answer[key].clone()
        };
        data::set(
            &mut state,
            &format!("{output}/{}", data::escape_token(key)),
            value,
        )?;
    }
    run.state = state;
    for item in &mut run.progress[run.cursor].items {
        if keys.contains(&item.key) {
            item.done = true;
        }
    }
    Ok(())
}

fn invalidate(run: &mut Run, target: usize, reset_visits: bool) -> Result<()> {
    for (_, step) in run.spec.workflow.steps.iter().skip(target) {
        if let Some(output) = &step.output {
            data::remove(&mut run.state, output)?;
        }
    }
    for task in &mut run.tasks {
        if task.step_index >= target {
            task.phase = "stale".into();
        }
    }
    for progress in run.progress.iter_mut().skip(target) {
        progress.generation = progress
            .generation
            .checked_add(1)
            .context("step generation overflow")?;
        if reset_visits {
            progress.visits = 0;
        }
        progress.phase = "pending".into();
        progress.initialized = false;
        progress.items.clear();
    }
    run.cursor = target;
    run.status = "waiting".into();
    run.errors.clear();
    Ok(())
}

fn rewind(run: &mut Run, name: &str, accept_workflow: bool) -> Result<()> {
    let latest = config::load(&run.workflow_path)?;
    if latest.digest == run.spec.digest {
        let target = run
            .spec
            .workflow
            .steps
            .get_index_of(name)
            .with_context(|| format!("unknown step {name}"))?;
        ensure!(
            target <= run.cursor,
            "rewind cannot skip unfinished steps; target must be the current or an earlier step"
        );
        return invalidate(run, target, true);
    }
    ensure!(
        accept_workflow,
        "workflow or schema changed; rewind requires --accept-workflow to adopt it"
    );
    let target = latest
        .workflow
        .steps
        .get_index_of(name)
        .with_context(|| format!("unknown step in updated workflow: {name}"))?;
    // Every preserved prefix step must retain its name, definition and schema.
    for index in 0..target {
        let (new_name, new_step) = latest
            .workflow
            .steps
            .get_index(index)
            .expect("prefix index");
        let Some((old_name, old_step)) = run.spec.workflow.steps.get_index(index) else {
            bail!(
                "rewind before the earliest changed step; updated prefix is longer than original"
            );
        };
        ensure!(new_name == old_name && new_step == old_step && latest.schemas.get(new_name) == run.spec.schemas.get(old_name),
            "step {old_name} changed before rewind target; rewind {new_name} --accept-workflow instead");
        ensure!(
            run.progress[index].phase == "completed",
            "rewind cannot preserve unfinished prefix step {old_name}"
        );
    }
    // Invalidate old outputs before replacing definitions, then new output paths.
    invalidate(run, target.min(run.progress.len()), true)?;
    for (_, step) in latest.workflow.steps.iter().skip(target) {
        if let Some(output) = &step.output {
            data::remove(&mut run.state, output)?;
        }
    }
    let mut progress: Vec<Progress> = run.progress.iter().take(target).cloned().collect();
    let generation = run
        .tasks
        .iter()
        .map(|t| t.generation)
        .chain(run.progress.iter().map(|p| p.generation))
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .context("step generation overflow")?;
    for name in latest.workflow.steps.keys().skip(target) {
        let mut p = Progress::new(name.clone());
        // Definitions can be adopted repeatedly, including removing and adding
        // steps. Never reuse a generation from any earlier task snapshot.
        p.generation = generation;
        progress.push(p);
    }
    run.spec = latest;
    run.progress = progress;
    run.cursor = target;
    Ok(())
}

fn rejected(run: &Run, db: &Path, errors: Vec<Value>) -> Result<Value> {
    let mut response = describe(run, db)?;
    response["status"] = json!("rejected");
    response["errors"] = json!(errors);
    Ok(response)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().context("task artifact directory missing")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(file.as_file_mut(), value)?;
    file.write_all(b"\n")?;
    file.persist(path)
        .map_err(|e| e.error)
        .context("export task artifact")?;
    Ok(())
}

fn describe(run: &Run, db: &Path) -> Result<Value> {
    describe_with_artifacts(run, db, true)
}

fn describe_with_artifacts(run: &Run, db: &Path, export: bool) -> Result<Value> {
    let mut pending = vec![];
    let task_root = db
        .parent()
        .context("database directory missing")?
        .join("tasks")
        .join(&run.id);
    for task in run.tasks.iter().filter(|task| task.phase == "pending") {
        let directory = task_root.join(&task.id);
        let context_file = directory.join("context.json");
        let schema_file = directory.join("schema.json");
        if export {
            write_json(&context_file, &task.context)?;
            write_json(&schema_file, &task.output_schema)?;
        }
        let submit_command = format!(
            "skflow --db {} submit {} --file 'answer.json'",
            shell_quote(&db.to_string_lossy()),
            shell_quote(&task.id)
        );
        pending.push(json!({
            "task_id":task.id,"run_id":run.id,"step":task.step,"generation":task.generation,
            "instruction":task.instruction,"keys":task.keys,"context":task.context,
            "output_schema":task.output_schema,"context_file":context_file,"schema_file":schema_file,
            "submit_command":submit_command,
            "submission_format":if task.foreach { "object keyed by exactly the assigned keys; each value follows the item schema" } else { "JSON value matching output_schema" },
        }));
    }
    let steps: Vec<Value> = run.progress.iter().enumerate().map(|(index, progress)| json!({
        "name":progress.name,"phase":progress.phase,"visits":progress.visits,"generation":progress.generation,
        "total_items":progress.items.len(),"completed_items":progress.items.iter().filter(|i| i.done).count(),
        "pending_tasks":run.tasks.iter().filter(|t| t.step_index==index && t.phase=="pending").count(),
    })).collect();
    let mut response = json!({
        "status":run.status,"run_id":run.id,"workflow":run.spec.workflow.name,
        "workflow_file":run.workflow_path,"workflow_digest":run.spec.digest,"db":db,
        "current_step":run.progress.get(run.cursor).map(|p| &p.name),
        "progress":{"completed_steps":run.progress.iter().filter(|p|p.phase=="completed").count(),"total_steps":run.progress.len(),"steps":steps},
        "pending_tasks":pending,"errors":run.errors,
    });
    if run.status == "completed" {
        let mut outputs = Map::new();
        for step in run.spec.workflow.steps.values() {
            if let Some(pointer) = &step.output {
                outputs.insert(pointer.clone(), data::get(&run.state, pointer)?);
            }
        }
        response["outputs"] = Value::Object(outputs);
    }
    Ok(response)
}
