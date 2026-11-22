use super::{MAX_PARALLEL, Registry};
use anyhow::{Result, bail, ensure};
use jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Deserialize)]
pub(super) struct BatchInput {
    pub(super) tool_calls: Vec<ToolCallInput>,
}

#[derive(Deserialize)]
pub(super) struct ToolCallInput {
    pub(super) tool: String,
    pub(super) parameters: Option<Value>,
}

impl ToolCallInput {
    pub(super) fn resolved_parameters(self) -> (String, Value) {
        (
            self.tool,
            self.parameters.unwrap_or_else(|| Value::Object(Map::new())),
        )
    }
}

struct Selector {
    route: String,
    schema: Value,
}

impl BatchInput {
    pub(super) async fn for_session(
        input: Value,
        registry: &Registry,
        session_id: &str,
    ) -> Result<Self> {
        if let Some(calls) = input.get("tool_calls").and_then(Value::as_array) {
            ensure!(!calls.is_empty(), "No tool calls provided");
            ensure!(
                calls.len() <= MAX_PARALLEL,
                "Maximum {MAX_PARALLEL} parallel tool calls allowed"
            );
        }
        let selectors = {
            let configured = super::super::sdk::config(session_id);
            let tools = registry.tools.read().await;
            let mut selectors = HashMap::new();
            for call in input
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for candidate in ["tool", "name"]
                    .into_iter()
                    .filter_map(|key| call.get(key)?.as_str())
                {
                    let route = Registry::resolve_tool_name_for_session(session_id, candidate);
                    let schema = configured
                        .as_ref()
                        .and_then(|config| config.custom.iter().find(|tool| tool.name == route))
                        .map(|tool| tool.parameters.clone())
                        .or_else(|| tools.get(route).map(|tool| tool.parameters_schema()));
                    if let Some(schema) = schema {
                        selectors.insert(
                            candidate.to_owned(),
                            Selector {
                                route: route.to_owned(),
                                schema,
                            },
                        );
                    }
                }
            }
            selectors
        };
        Ok(serde_json::from_value(normalize(input, &selectors)?)?)
    }
}

fn normalize(mut input: Value, selectors: &HashMap<String, Selector>) -> Result<Value> {
    if let Some(calls) = input.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for object in calls.iter_mut().filter_map(Value::as_object_mut) {
            *object = normalize_child(std::mem::take(object), selectors)?;
        }
    }
    Ok(input)
}

fn normalize_child(
    mut child: Map<String, Value>,
    selectors: &HashMap<String, Selector>,
) -> Result<Map<String, Value>> {
    let mut parameters = child.remove("parameters").filter(|value| !value.is_null());
    let mut tool = child.remove("tool");
    if tool.is_none() {
        tool = child.remove("name");
    } else if parameters.is_none()
        && let (Some(explicit), Some(name)) = (
            tool.as_ref().and_then(Value::as_str),
            child.get("name").and_then(Value::as_str),
        )
    {
        let direct = selectors.get(explicit);
        let alias = selectors.get(name);
        let same = match (direct, alias) {
            (Some(direct), Some(alias)) => direct.route == alias.route,
            _ => explicit == name,
        };
        if !may_be_native(direct, "name") {
            if same {
                child.remove("name");
            } else if direct.is_some() && alias.is_some() {
                bail!(
                    "Ambiguous batch tool/name selectors. Put the child input inside `parameters`."
                );
            } else if alias.is_some_and(|alias| {
                matches!(alias.route.as_str(), "mcp_call" | "mcp")
                    && property_declaration(&alias.schema, "tool") == Some(true)
            }) {
                // Legacy MCP calls can name the wrapper with `name` and the raw
                // server tool with `tool`. Only recover an unambiguous route.
                if let Some(wrapper) = child.remove("name")
                    && let Some(raw_tool) = tool.replace(wrapper)
                {
                    child.insert("tool".into(), raw_tool);
                }
            }
        }
    }

    let intent = child
        .get("intent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|intent| !intent.is_empty())
        .map(str::to_owned);
    let accept = child
        .get(ACCEPT_LARGE_OUTPUT_KEY)
        .filter(|value| !value.is_null())
        .cloned();
    if parameters.is_none() {
        let selector = tool
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|tool| selectors.get(tool));
        if let Some(alias) = ["arguments", "args", "input"].into_iter().find(|alias| {
            child.contains_key(*alias)
                && !may_be_native(selector, alias)
                && child.keys().all(|key| {
                    key.as_str() == *alias
                        || matches!(key.as_str(), "intent" | ACCEPT_LARGE_OUTPUT_KEY)
                })
        }) {
            parameters = child.remove(alias);
        } else if !child.is_empty() {
            parameters = Some(Value::Object(child));
        }
    }

    if let Some(Value::Object(child)) = &mut parameters {
        if let Some(intent) = intent {
            child.insert("intent".into(), Value::String(intent));
        }
        if let Some(accept) = accept {
            child.entry(ACCEPT_LARGE_OUTPUT_KEY).or_insert(accept);
        }
    }

    let mut normalized = Map::new();
    if let Some(tool) = tool {
        normalized.insert("tool".into(), tool);
    }
    if let Some(parameters) = parameters {
        normalized.insert("parameters".into(), parameters);
    }
    Ok(normalized)
}

fn may_be_native(selector: Option<&Selector>, property: &str) -> bool {
    selector.is_some_and(|selector| property_declaration(&selector.schema, property) != Some(false))
}

// None means traversal could not rule out a native field. In that case keep the
// field rather than guessing that it is a selector or the whole child payload.
fn property_declaration(schema: &Value, property: &str) -> Option<bool> {
    fn visit(
        schema: &Value,
        root: &Value,
        property: &str,
        depth: usize,
        left: &mut usize,
    ) -> Option<bool> {
        if depth > 16 || *left == 0 {
            return None;
        }
        *left -= 1;
        if schema
            .get("properties")
            .is_some_and(|properties| properties.get(property).is_some())
        {
            return Some(true);
        }
        let mut unknown = schema == true
            || ["additionalProperties", "unevaluatedProperties"]
                .into_iter()
                .any(|key| matches!(schema.get(key), Some(Value::Bool(true) | Value::Object(_))))
            || schema.get("patternProperties").is_some_and(|patterns| {
                patterns
                    .as_object()
                    .is_none_or(|patterns| !patterns.is_empty())
            })
            || schema.get("$dynamicRef").is_some()
            || schema.get("$recursiveRef").is_some();
        let mut check = |branch| match visit(branch, root, property, depth + 1, left) {
            Some(true) => true,
            Some(false) => false,
            None => {
                unknown = true;
                false
            }
        };
        if let Some(reference) = schema.get("$ref") {
            let target = reference
                .as_str()
                .and_then(|reference| reference.strip_prefix('#'))
                .and_then(|pointer| root.pointer(pointer));
            if check(target.unwrap_or(&Value::Bool(true))) {
                return Some(true);
            }
        }
        for key in ["allOf", "anyOf", "oneOf", "if", "then", "else"] {
            if let Some(branch) = schema.get(key) {
                let branches = branch
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(std::slice::from_ref(branch));
                if branches.iter().any(&mut check) {
                    return Some(true);
                }
            }
        }
        if let Some(dependencies) = schema.get("dependentSchemas") {
            match dependencies.as_object() {
                Some(dependencies) if dependencies.values().any(&mut check) => return Some(true),
                None => unknown = true,
                _ => {}
            }
        }
        (!unknown).then_some(false)
    }
    visit(schema, schema, property, 0, &mut 256)
}

#[cfg(test)]
pub(super) fn normalize_batch_input(input: Value) -> Value {
    normalize(input, &HashMap::new()).unwrap()
}
