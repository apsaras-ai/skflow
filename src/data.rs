//! JSON Pointer addressing, deterministic context projection, and offline schemas.

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// Decode an RFC 6901 JSON Pointer, rejecting malformed `~` escape sequences.
pub fn pointer_tokens(pointer: &str) -> Result<Vec<String>> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        pointer.starts_with('/'),
        "JSON Pointer must start with '/': {pointer}"
    );
    pointer[1..]
        .split('/')
        .map(|part| {
            let mut result = String::new();
            let mut chars = part.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '~' => match chars.next() {
                        Some('0') => result.push('~'),
                        Some('1') => result.push('/'),
                        _ => bail!("invalid JSON Pointer escape in {pointer}"),
                    },
                    ch => result.push(ch),
                }
            }
            Ok(result)
        })
        .collect()
}

pub fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

pub fn get(state: &Value, pointer: &str) -> Result<Value> {
    let mut current = state;
    for token in pointer_tokens(pointer)? {
        current = match current {
            Value::Object(object) => object.get(&token),
            Value::Array(array) => {
                ensure!(
                    token == "0"
                        || (!token.starts_with('0')
                            && !token.is_empty()
                            && token.bytes().all(|b| b.is_ascii_digit())),
                    "invalid array index '{token}' in {pointer}"
                );
                array.get(token.parse::<usize>().context("array index is too large")?)
            }
            _ => None,
        }
        .ok_or_else(|| anyhow!("state path does not exist: {pointer}"))?;
    }
    Ok(current.clone())
}

/// Set only an object-owned path. Never replace an intermediate scalar or array.
/// Validation is performed before mutation so a failed write leaves state intact.
pub fn set(state: &mut Value, pointer: &str, value: Value) -> Result<()> {
    let tokens = pointer_tokens(pointer)?;
    ensure!(!tokens.is_empty(), "writing the state root is forbidden");
    let mut probe = &*state;
    for (index, token) in tokens.iter().enumerate() {
        let object = probe
            .as_object()
            .ok_or_else(|| anyhow!("output path crosses a non-object at {pointer}"))?;
        if index + 1 == tokens.len() {
            break;
        }
        match object.get(token) {
            Some(next) => probe = next,
            None => break,
        }
    }
    let mut current = state;
    for token in &tokens[..tokens.len() - 1] {
        current = current
            .as_object_mut()
            .expect("prevalidated object path")
            .entry(token.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .expect("prevalidated object path")
        .insert(tokens.last().expect("nonempty pointer").clone(), value);
    Ok(())
}

/// Remove an output if present. Missing descendants are already cleared.
pub fn remove(state: &mut Value, pointer: &str) -> Result<()> {
    let tokens = pointer_tokens(pointer)?;
    ensure!(!tokens.is_empty(), "removing the state root is forbidden");
    let mut current = state;
    for token in &tokens[..tokens.len() - 1] {
        let Some(next) = current
            .as_object_mut()
            .and_then(|object| object.get_mut(token))
        else {
            return Ok(());
        };
        current = next;
    }
    if let Some(object) = current.as_object_mut() {
        object.remove(tokens.last().expect("nonempty pointer"));
    }
    Ok(())
}

/// Project a template against state. Placeholders are values, never stringified JSON.
pub fn project(
    template: &Value,
    state: &Value,
    item: Option<&Value>,
    key: Option<&str>,
) -> Result<Value> {
    match template {
        Value::String(text) if text == "{item}" => item
            .cloned()
            .ok_or_else(|| anyhow!("{{item}} is only available inside foreach")),
        Value::String(text) if text == "{key}" => key
            .map(|key| Value::String(key.to_owned()))
            .ok_or_else(|| anyhow!("{{key}} is only available inside foreach")),
        Value::String(text) if text.starts_with('/') => {
            let pointer = if text.contains("{key}") {
                text.replace(
                    "{key}",
                    &escape_token(key.ok_or_else(|| anyhow!("{{key}} requires foreach"))?),
                )
            } else {
                text.clone()
            };
            get(state, &pointer)
        }
        Value::Array(values) => values
            .iter()
            .map(|value| project(value, state, item, key))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(object) => object
            .iter()
            .map(|(name, value)| Ok((name.clone(), project(value, state, item, key)?)))
            .collect::<Result<Map<_, _>>>()
            .map(Value::Object),
        other => Ok(other.clone()),
    }
}

/// Compile without any network or filesystem resolver. The Cargo feature set also
/// disables the default resolvers; the explicit retriever keeps this invariant if
/// another dependency later enables those features through feature unification.
pub fn compile_schema(schema: &Value) -> Result<jsonschema::Validator> {
    struct Offline;
    impl jsonschema::Retrieve for Offline {
        fn retrieve(
            &self,
            uri: &jsonschema::Uri<String>,
        ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
            Err(format!("external schema references are disabled: {uri}").into())
        }
    }
    let rooted = rooted_schema(schema)?;
    jsonschema::options()
        .with_retriever(Offline)
        .build(&rooted)
        .map_err(|error| anyhow!("invalid or non-self-contained JSON Schema: {error}"))
}

/// A local schema file has no retrieval URI inside a restored task. Give relative
/// root IDs a stable virtual absolute identity without performing any retrieval.
fn rooted_schema(schema: &Value) -> Result<Value> {
    let draft4 = schema
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(|draft| draft.contains("draft-04"));
    let id_keyword = if draft4 { "id" } else { "$id" };
    let mut rooted = schema.clone();
    if let Some(id) = schema.get(id_keyword).and_then(Value::as_str) {
        let base = fluent_uri::Uri::parse("https://skflow.invalid/schema.json")?;
        let reference = fluent_uri::UriRef::parse(id).context("invalid JSON Schema resource ID")?;
        let absolute = reference
            .resolve_against(&base)
            .context("invalid JSON Schema resource ID")?;
        rooted[id_keyword] = Value::String(absolute.as_str().to_owned());
    }
    Ok(rooted)
}

/// Drafts 4/6/7 ignore siblings of `$ref`, including `$id`. A root-ref schema
/// therefore needs a separate resource container. Relocate only references in
/// the original root scope; data in enum/default/examples is never rewritten.
fn legacy_ref_resource(schema: &Value, id: &str, draft: jsonschema::Draft) -> Result<Value> {
    fn collect(
        root: &Value,
        node: &Value,
        draft: jsonschema::Draft,
        visited: &mut HashSet<usize>,
    ) -> Result<()> {
        if !std::ptr::eq(root, node) && draft.create_resource_ref(node).id().is_some() {
            return Ok(());
        }
        if !visited.insert(node as *const Value as usize) {
            return Ok(());
        }
        if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
            if reference.starts_with('#') {
                let uri = fluent_uri::UriRef::parse(reference)?;
                if let Some(fragment) = uri.fragment() {
                    let pointer = fragment.decode().into_string()?;
                    if pointer.is_empty() || pointer.starts_with('/') {
                        if let Some(target) = root.pointer(&pointer) {
                            collect(root, target, draft, visited)?;
                        }
                    }
                }
            }
        }
        for child in draft.subresources_of(node) {
            collect(root, child, draft, visited)?;
        }
        Ok(())
    }

    fn relocate(original: &Value, copy: &mut Value, schemas: &HashSet<usize>) -> Result<()> {
        if schemas.contains(&(original as *const Value as usize)) {
            if let Some(reference) = original.get("$ref").and_then(Value::as_str) {
                if let Some(fragment_text) = reference.strip_prefix('#') {
                    let uri = fluent_uri::UriRef::parse(reference)?;
                    if let Some(fragment) = uri.fragment() {
                        let pointer = fragment.decode().into_string()?;
                        if pointer.is_empty() || pointer.starts_with('/') {
                            copy["$ref"] =
                                Value::String(format!("#/definitions/value{fragment_text}"));
                        }
                    }
                }
            }
        }
        match (original, copy) {
            (Value::Object(original), Value::Object(copy)) => {
                for (key, value) in original {
                    relocate(value, copy.get_mut(key).expect("cloned key"), schemas)?;
                }
            }
            (Value::Array(original), Value::Array(copy)) => {
                for (original, copy) in original.iter().zip(copy) {
                    relocate(original, copy, schemas)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut schemas = HashSet::new();
    collect(schema, schema, draft, &mut schemas)?;
    let mut relocated = schema.clone();
    relocate(schema, &mut relocated, &schemas)?;
    let mut resource = json!({
        "$schema": schema["$schema"],
        "definitions": {"value": relocated},
        "allOf": [{"$ref":"#/definitions/value"}],
    });
    resource[if draft == jsonschema::Draft::Draft4 {
        "id"
    } else {
        "$id"
    }] = json!(id);
    Ok(resource)
}

pub fn validate(schema: &Value, value: &Value) -> Result<Vec<Value>> {
    let validator = compile_schema(schema)?;
    Ok(validator
        .iter_errors(value)
        .map(|error| {
            json!({
                "instance_path": error.instance_path.to_string(),
                "schema_path": error.schema_path.to_string(),
                "message": error.to_string(),
            })
        })
        .collect())
}

/// Build a complete submission schema whose property names are the assigned keys.
/// The item schema gets its own resource scope so `#` and `#/$defs/...` continue to
/// address the item schema, including recursive schemas, instead of the batch root.
pub fn batch_output_schema(item_schema: &Value, keys: &[String]) -> Result<Value> {
    compile_schema(item_schema)?;
    let mut seen = HashSet::new();
    ensure!(
        keys.iter().all(|key| seen.insert(key)),
        "duplicate task key"
    );
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(item_schema)?));
    let synthetic_id = format!("https://skflow.invalid/schemas/{digest}/item.json");
    let wrapper_id = format!("https://skflow.invalid/schemas/{digest}/batch.json");
    let draft = item_schema
        .get("$schema")
        .and_then(Value::as_str)
        .unwrap_or("https://json-schema.org/draft/2020-12/schema");
    let draft4 = draft.contains("draft-04");
    let dialect = jsonschema::Draft::default().detect(item_schema)?;
    let mut embedded = if dialect <= jsonschema::Draft::Draft7 && item_schema.get("$ref").is_some()
    {
        legacy_ref_resource(item_schema, &synthetic_id, dialect)?
    } else {
        rooted_schema(item_schema)?
    };
    let reference = if let Some(object) = embedded.as_object_mut() {
        let id_keyword = if draft4 { "id" } else { "$id" };
        // Preserve an existing resource identity, including relative identities:
        // a self-reference such as "item.json#/$defs/value" must still resolve.
        // Relative identifiers use the wrapper's stable synthetic base below.
        let original = object.get(id_keyword).and_then(Value::as_str);
        if let Some(id) = original.filter(|id| !id.is_empty() && !id.starts_with('#')) {
            id.to_owned()
        } else {
            object.insert(id_keyword.to_owned(), Value::String(synthetic_id.clone()));
            synthetic_id.clone()
        }
    } else {
        // Boolean schemas cannot carry a resource identifier; a pointer is enough.
        "#/definitions/item".to_owned()
    };
    let properties: Map<String, Value> = keys
        .iter()
        .map(|key| (key.clone(), json!({"$ref": reference})))
        .collect();
    let mut schema = json!({
        "$schema": draft,
        "type": "object",
        "properties": properties,
        "required": keys,
        "additionalProperties": false,
        "definitions": {"item": embedded},
    });
    schema[if draft4 { "id" } else { "$id" }] = Value::String(wrapper_id);
    compile_schema(&schema).context("cannot wrap item schema for a batch")?;
    Ok(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_escape_and_preserve_literal_keys() {
        let mut state = json!({"raw": {"stable": true}});
        let key = "x/~1/..";
        let path = format!("/reviews/{}", escape_token(key));
        set(&mut state, &path, json!({"ok": true})).unwrap();
        assert_eq!(get(&state, &path).unwrap(), json!({"ok": true}));
        assert_eq!(state["reviews"][key]["ok"], true);
        assert_eq!(state["raw"], json!({"stable": true}));
        assert!(pointer_tokens("/broken~2").is_err());
        assert!(pointer_tokens("/broken~").is_err());
        assert_eq!(pointer_tokens("/~01").unwrap(), vec!["~1"]);
    }

    #[test]
    fn cannot_replace_root_or_cross_scalar_and_array() {
        let original = json!({"a": {"b": 2}, "items": [1, 2]});
        for pointer in ["", "/a/b/c", "/items/0/x"] {
            let mut state = original.clone();
            assert!(set(&mut state, pointer, json!(true)).is_err());
            assert_eq!(state, original);
        }
        assert_eq!(get(&original, "/items/0").unwrap(), 1);
        assert!(get(&original, "/items/01").is_err());
        assert!(get(&original, "/items/-").is_err());
    }

    #[test]
    fn projection_is_typed_and_escapes_key() {
        let state = json!({"enriched": {"a/b~": {"score": 4}}, "secret": "not projected"});
        let item = json!({"id": "a/b~", "name": "Ada"});
        let context = project(
            &json!({"candidate": "{item}", "key": "{key}", "enriched": "/enriched/{key}"}),
            &state,
            Some(&item),
            Some("a/b~"),
        )
        .unwrap();
        assert_eq!(
            context,
            json!({"candidate": item, "key": "a/b~", "enriched": {"score": 4}})
        );
        assert!(project(&json!("/missing"), &state, None, None).is_err());
        assert!(project(&json!("{item}"), &state, None, None).is_err());
    }

    #[test]
    fn batch_requires_exact_keys_and_preserves_item_refs() {
        let item = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"text": {"type": "string", "minLength": 4}},
            "type": "object",
            "required": ["reason"],
            "properties": {"reason": {"$ref": "#/$defs/text"}},
            "additionalProperties": false
        });
        let schema = batch_output_schema(&item, &["a/~".into(), "b".into()]).unwrap();
        assert!(validate(
            &schema,
            &json!({"a/~": {"reason": "good"}, "b": {"reason": "also good"}})
        )
        .unwrap()
        .is_empty());
        for answer in [
            json!({"a/~": {"reason": "good"}}),
            json!({"a/~": {"reason": "good"}, "b": {"reason": "no"}}),
            json!({"a/~": {"reason": "good"}, "b": {"reason": "good"}, "foreign": {"reason": "good"}}),
        ] {
            assert!(!validate(&schema, &answer).unwrap().is_empty());
        }
    }

    #[test]
    fn recursive_and_legacy_schemas_keep_their_scope() {
        for draft in [
            "http://json-schema.org/draft-07/schema#",
            "http://json-schema.org/draft-04/schema#",
        ] {
            let item = json!({
                "$schema": draft,
                "type": "object",
                "properties": {"next": {"$ref": "#"}, "count": {"type": "integer"}},
                "required": ["count"]
            });
            let schema = batch_output_schema(&item, &["a".into()]).unwrap();
            assert!(
                validate(&schema, &json!({"a": {"count": 1, "next": {"count": 2}}}))
                    .unwrap()
                    .is_empty()
            );
            assert!(!validate(
                &schema,
                &json!({"a": {"count": 1, "next": {"count": "wrong"}}})
            )
            .unwrap()
            .is_empty());
        }
    }

    #[test]
    fn schemas_cannot_retrieve_external_documents() {
        assert!(compile_schema(&json!({"$ref": "https://example.org/remote.json"})).is_err());
        assert!(compile_schema(&json!({"$ref": "file:///tmp/private-schema.json"})).is_err());
    }

    #[test]
    fn batch_preserves_relative_resource_self_references() {
        let item = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "review.json",
            "definitions": {"reason": {"type": "string", "minLength": 3}},
            "type": "object",
            "required": ["reason"],
            "properties": {"reason": {"$ref": "review.json#/definitions/reason"}}
        });
        let schema = batch_output_schema(&item, &["a".into()]).unwrap();
        assert!(validate(&schema, &json!({"a": {"reason": "good"}}))
            .unwrap()
            .is_empty());
        assert!(!validate(&schema, &json!({"a": {"reason": "x"}}))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn batch_supports_legacy_root_ref_without_activating_ignored_siblings() {
        for draft in [
            "http://json-schema.org/draft-04/schema#",
            "http://json-schema.org/draft-07/schema#",
        ] {
            let item = json!({
                "$schema": draft,
                "$ref": "#/definitions/result",
                "type": "integer",
                "definitions": {
                    "result": {
                        "type": "object",
                        "required": ["reason"],
                        "properties": {"reason": {"$ref": "#/definitions/text"}},
                        "additionalProperties": false
                    },
                    "text": {"type": "string", "minLength": 4}
                }
            });
            let schema = batch_output_schema(&item, &["a".into()]).unwrap();
            assert!(validate(&schema, &json!({"a": {"reason": "good"}}))
                .unwrap()
                .is_empty());
            assert!(!validate(&schema, &json!({"a": {"reason": "no"}}))
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn relocating_legacy_refs_does_not_modify_literal_enum_data() {
        let item = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$ref": "#/definitions/value",
            "definitions": {"value": {"enum": [{"$ref":"#/literal"}]}}
        });
        let schema = batch_output_schema(&item, &["a".into()]).unwrap();
        assert!(validate(&schema, &json!({"a": {"$ref":"#/literal"}}))
            .unwrap()
            .is_empty());
    }
}
