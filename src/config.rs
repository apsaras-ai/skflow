//! Ordered workflow definitions and startup validation.

use crate::data::{compile_schema, pointer_tokens};
use anyhow::{anyhow, bail, ensure, Context, Result};
use indexmap::IndexMap;
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{fmt, fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub version: u32,
    pub name: String,
    #[serde(deserialize_with = "ordered_unique_steps")]
    pub steps: IndexMap<String, Step>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub run: Option<String>,
    pub agent: Option<Agent>,
    pub check: Option<Vec<Assertion>>,
    pub foreach: Option<ForEach>,
    pub output: Option<String>,
    pub timeout: Option<String>,
    #[serde(default)]
    pub retry: u32,
    pub input: Option<Value>,
    pub on_fail: Option<OnFail>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub instruction: String,
    #[serde(default = "empty_object")]
    pub context: Value,
    pub schema: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    pub expr: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ForEach {
    pub items: String,
    pub key: String,
    #[serde(default = "one")]
    pub batch: usize,
    #[serde(default = "one")]
    pub concurrency: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OnFail {
    pub goto: String,
    pub max_visits: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoadedWorkflow {
    pub workflow: Workflow,
    /// Schema documents keyed by step name, frozen for this run.
    pub schemas: IndexMap<String, Value>,
    pub digest: String,
}

fn one() -> usize {
    1
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn ordered_unique_steps<'de, D>(
    deserializer: D,
) -> std::result::Result<IndexMap<String, Step>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Unique;
    impl<'de> de::Visitor<'de> for Unique {
        type Value = IndexMap<String, Step>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an ordered mapping of unique step names")
        }

        fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let mut steps = IndexMap::new();
            while let Some((name, step)) = map.next_entry::<String, Step>()? {
                if steps.contains_key(&name) {
                    return Err(de::Error::custom(format!("duplicate step name: {name}")));
                }
                steps.insert(name, step);
            }
            Ok(steps)
        }
    }
    deserializer.deserialize_map(Unique)
}

pub fn load(path: &Path) -> Result<LoadedWorkflow> {
    let yaml =
        fs::read(path).with_context(|| format!("cannot read workflow {}", path.display()))?;
    let workflow: Workflow = serde_yaml::from_slice(&yaml).context("invalid workflow YAML")?;
    validate_workflow(&workflow)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut schemas = IndexMap::new();
    let mut digest = Sha256::new();
    digest.update((yaml.len() as u64).to_le_bytes());
    digest.update(&yaml);
    for (name, step) in &workflow.steps {
        if let Some(agent) = &step.agent {
            ensure!(
                !agent.schema.trim().is_empty(),
                "step {name}: schema path is empty"
            );
            let schema_path = base.join(&agent.schema);
            let bytes = fs::read(&schema_path).with_context(|| {
                format!("step {name}: cannot read schema {}", schema_path.display())
            })?;
            let schema: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("step {name}: invalid schema JSON"))?;
            compile_schema(&schema).with_context(|| format!("step {name}"))?;
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(&bytes);
            schemas.insert(name.clone(), schema);
        }
    }
    Ok(LoadedWorkflow {
        workflow,
        schemas,
        digest: format!("{:x}", digest.finalize()),
    })
}

fn validate_workflow(workflow: &Workflow) -> Result<()> {
    ensure!(
        workflow.version == 1,
        "unsupported workflow version: {}",
        workflow.version
    );
    ensure!(
        !workflow.name.trim().is_empty(),
        "workflow name must not be empty"
    );
    ensure!(
        !workflow.steps.is_empty(),
        "workflow must contain at least one step"
    );
    let mut outputs: Vec<(&str, Vec<String>)> = Vec::new();
    for (index, (name, step)) in workflow.steps.iter().enumerate() {
        ensure!(!name.trim().is_empty(), "step name must not be empty");
        let count = usize::from(step.run.is_some())
            + usize::from(step.agent.is_some())
            + usize::from(step.check.is_some());
        ensure!(
            count == 1,
            "step {name}: exactly one of run, agent, check is required"
        );
        if let Some(command) = &step.run {
            ensure!(
                !command.trim().is_empty(),
                "step {name}: run command must not be empty"
            );
        }
        if let Some(agent) = &step.agent {
            ensure!(
                !agent.instruction.trim().is_empty(),
                "step {name}: agent instruction must not be empty"
            );
        }
        if step.run.is_none() {
            ensure!(
                step.timeout.is_none() && step.retry == 0 && step.input.is_none(),
                "step {name}: timeout, retry, and input apply only to run steps"
            );
        }
        ensure!(step.retry <= 100, "step {name}: retry must be at most 100");
        if let Some(timeout) = &step.timeout {
            let duration = humantime::parse_duration(timeout)
                .with_context(|| format!("step {name}: invalid timeout"))?;
            ensure!(
                !duration.is_zero(),
                "step {name}: timeout must be greater than zero"
            );
        }
        if let Some(assertions) = &step.check {
            ensure!(
                step.output.is_none() && step.foreach.is_none(),
                "step {name}: check cannot have output or foreach"
            );
            ensure!(
                !assertions.is_empty(),
                "step {name}: check must contain at least one assertion"
            );
            for assertion in assertions {
                ensure!(
                    !assertion.message.trim().is_empty(),
                    "step {name}: check message must not be empty"
                );
                jmespath::compile(&assertion.expr)
                    .map_err(|error| anyhow!("step {name}: invalid JMESPath: {error}"))?;
            }
            if let Some(on_fail) = &step.on_fail {
                let target = workflow
                    .steps
                    .get_index_of(&on_fail.goto)
                    .ok_or_else(|| anyhow!("step {name}: unknown goto target {}", on_fail.goto))?;
                ensure!(
                    target < index,
                    "step {name}: goto must target an earlier step"
                );
                ensure!(
                    on_fail.max_visits >= 1,
                    "step {name}: max_visits must be at least 1"
                );
            }
        } else {
            ensure!(
                step.on_fail.is_none(),
                "step {name}: on_fail applies only to check steps"
            );
            let output = step
                .output
                .as_deref()
                .ok_or_else(|| anyhow!("step {name}: output is required"))?;
            let tokens =
                pointer_tokens(output).with_context(|| format!("step {name}: invalid output"))?;
            ensure!(
                !tokens.is_empty(),
                "step {name}: output cannot be the state root"
            );
            ensure!(
                tokens[0] != "input",
                "step {name}: /input is reserved and cannot be an output"
            );
            for (other_name, other_tokens) in &outputs {
                if tokens.starts_with(other_tokens) || other_tokens.starts_with(&tokens) {
                    bail!(
                        "step {name}: output {output} overlaps with output owned by {other_name}"
                    );
                }
            }
            outputs.push((name, tokens));
        }
        if let Some(each) = &step.foreach {
            pointer_tokens(&each.items)
                .with_context(|| format!("step {name}: invalid foreach items pointer"))?;
            ensure!(
                !each.key.is_empty(),
                "step {name}: foreach key must be a nonempty object field name"
            );
            ensure!(
                (1..=1024).contains(&each.batch),
                "step {name}: foreach batch must be between 1 and 1024"
            );
            ensure!(
                (1..=1024).contains(&each.concurrency),
                "step {name}: foreach concurrency must be between 1 and 1024"
            );
            if let Some(output) = &step.output {
                let input_tokens = pointer_tokens(&each.items)?;
                let output_tokens = pointer_tokens(output)?;
                ensure!(
                    !(input_tokens.starts_with(&output_tokens)
                        || output_tokens.starts_with(&input_tokens)),
                    "step {name}: foreach input and output paths must not overlap"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn parse(yaml: &str) -> Result<Workflow> {
        let workflow: Workflow = serde_yaml::from_str(yaml)?;
        validate_workflow(&workflow)?;
        Ok(workflow)
    }

    #[test]
    fn yaml_order_is_execution_order_and_duplicates_are_rejected() {
        let workflow = parse("version: 1\nname: demo\nsteps:\n  z:\n    run: echo '{}'\n    output: /z\n  a:\n    run: echo '{}'\n    output: /a\n").unwrap();
        assert_eq!(
            workflow.steps.keys().cloned().collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert!(parse("version: 1\nname: demo\nsteps:\n  a:\n    run: x\n    output: /x\n  a:\n    run: y\n    output: /y\n").is_err());
    }

    #[test]
    fn output_owners_are_disjoint_by_decoded_tokens() {
        for second in ["/a", "/a/b"] {
            assert!(parse(&format!("version: 1\nname: demo\nsteps:\n  first:\n    run: x\n    output: /a\n  second:\n    run: x\n    output: {second}\n")).is_err());
        }
        assert!(parse("version: 1\nname: demo\nsteps:\n  first:\n    run: x\n    output: /a\n  second:\n    run: x\n    output: /ab\n").is_ok());
        assert!(parse("version: 1\nname: demo\nsteps:\n  first:\n    run: x\n    output: /a~1b\n  second:\n    run: x\n    output: /a/b\n").is_ok());
        assert!(parse(
            "version: 1\nname: demo\nsteps:\n  first:\n    run: x\n    output: /input/a\n"
        )
        .is_err());
    }

    #[test]
    fn checks_require_bounded_backward_edges() {
        let valid = "version: 1\nname: demo\nsteps:\n  run:\n    run: x\n    output: /x\n  gate:\n    check:\n      - expr: 'length(x) > `0`'\n        message: need values\n    on_fail: {goto: run, max_visits: 2}\n";
        assert!(parse(valid).is_ok());
        assert!(parse(&valid.replace("goto: run", "goto: gate")).is_err());
        assert!(parse(&valid.replace("max_visits: 2", "max_visits: 0")).is_err());
        assert!(parse(&valid.replace("length(x) > `0`", "bad(")).is_err());
    }

    #[test]
    fn invalid_combinations_and_zero_limits_fail_early() {
        for step in [
            "run: x\n    output: /x\n    timeout: 0s",
            "run: x\n    output: /x\n    foreach: {items: /input, key: id, batch: 0}",
            "run: x\n    output: /x\n    agent: {instruction: hello, schema: x.json}",
            "run: x\n    output: /x\n    typo: true",
            "run: x\n    output: /x\n    foreach: {items: /x/items, key: id}",
        ] {
            assert!(parse(&format!(
                "version: 1\nname: demo\nsteps:\n  work:\n    {step}\n"
            ))
            .is_err());
        }
    }

    #[test]
    fn workflow_digest_includes_schema_content_and_loads_from_workflow_directory() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("workflow.yaml");
        fs::write(&path, "version: 1\nname: demo\nsteps:\n  review:\n    agent: {instruction: review it, schema: review.json}\n    output: /reviews\n").unwrap();
        fs::write(
            directory.path().join("review.json"),
            r#"{"type":"string","minLength":3}"#,
        )
        .unwrap();
        let first = load(&path).unwrap();
        assert_eq!(first.schemas["review"]["minLength"], 3);
        fs::write(
            directory.path().join("review.json"),
            r#"{"type":"string","minLength":8}"#,
        )
        .unwrap();
        let second = load(&path).unwrap();
        assert_ne!(first.digest, second.digest);
        assert_eq!(first.schemas["review"]["minLength"], 3);
    }
}
