use super::*;

pub(super) fn apply_swarm_model_policy(schema: &mut Value, agents: &crate::config::AgentsConfig) {
    let Some(properties) = schema["properties"].as_object_mut() else {
        crate::logging::error(
            "Cannot apply swarm model policy: schema properties are not an object",
        );
        return;
    };
    // `agents.spawn_allowed_models` is the hard worker allowlist: advertise
    // exactly those entries so a model never proposes anything else at spawn
    // time. Without it, fall back to the legacy opt-in selection allowlist.
    match agents.worker_model_choices() {
        Some(choices) if choices.hard_boundary => {
            properties.insert(
                "model".to_string(),
                json!({
                    "type": "string",
                    "enum": choices.models,
                    "description": "Exact user-allowlisted worker model. Required for set_model; omit for the spawn default."
                }),
            );
        }
        Some(choices) => {
            properties.insert(
                "model".to_string(),
                json!({
                    "type": "string",
                    "enum": choices.models,
                    "description": "Exact user-allowlisted route. Required for set_model; omit for the spawn default."
                }),
            );
        }
        None => {
            properties.remove("model");
        }
    }
}
