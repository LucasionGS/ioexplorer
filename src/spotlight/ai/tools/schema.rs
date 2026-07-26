//! JSON Schema generated from a custom tool's typed parameters.
//!
//! Users declare parameters in TOML with a `type` from a small fixed set; the
//! schema the API needs is derived from that. Nobody writes JSON Schema in a
//! config file, and there is no way to declare a type that cannot be safely
//! substituted into a command line.
//!
//! Pure over the config types — no I/O — so the whole mapping is testable.

use crate::config::SpotlightAiToolParam;

/// Builds the `input_schema` for a tool's parameters.
///
/// `additionalProperties: false` is deliberate: the model has no reason to
/// invent a parameter, and a substitution step that silently ignores unknown
/// keys is harder to reason about than one that cannot receive them.
pub fn build(params: &[SpotlightAiToolParam]) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for param in params {
        let mut property = serde_json::Map::new();
        property.insert(
            "type".to_string(),
            serde_json::Value::String(param.kind.as_str().to_string()),
        );
        if let Some(description) = description(param) {
            property.insert(
                "description".to_string(),
                serde_json::Value::String(description),
            );
        }
        properties.insert(param.name.clone(), serde_json::Value::Object(property));

        if param.required {
            required.push(serde_json::Value::String(param.name.clone()));
        }
    }

    serde_json::json!({
        "type": "object",
        "properties": serde_json::Value::Object(properties),
        "required": serde_json::Value::Array(required),
        "additionalProperties": false,
    })
}

fn description(param: &SpotlightAiToolParam) -> Option<String> {
    param
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiParamType;

    fn param(name: &str, kind: AiParamType, required: bool) -> SpotlightAiToolParam {
        SpotlightAiToolParam {
            name: name.to_string(),
            kind,
            description: None,
            required,
        }
    }

    #[test]
    fn params_become_a_typed_object_schema() {
        let schema = build(&[
            SpotlightAiToolParam {
                description: Some("Song or artist".to_string()),
                ..param("query", AiParamType::String, true)
            },
            param("limit", AiParamType::Integer, false),
        ]);

        assert_eq!(
            schema,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Song or artist" },
                    "limit": { "type": "integer" },
                },
                "required": ["query"],
                "additionalProperties": false,
            })
        );
    }

    /// `required` must be present and an array even when nothing is required —
    /// a missing key is not the same as an empty list to a schema validator.
    #[test]
    fn a_tool_with_no_params_still_has_a_valid_schema() {
        let schema = build(&[]);

        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(schema["properties"], serde_json::json!({}));
        assert_eq!(schema["required"], serde_json::json!([]));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn every_param_type_maps_to_its_json_schema_name() {
        for (kind, expected) in [
            (AiParamType::String, "string"),
            (AiParamType::Integer, "integer"),
            (AiParamType::Number, "number"),
            (AiParamType::Boolean, "boolean"),
        ] {
            let schema = build(&[param("value", kind, false)]);
            assert_eq!(schema["properties"]["value"]["type"], expected);
        }
    }

    /// An omitted or blank description must not become `"description": ""` —
    /// an empty string is worse than no description at all.
    #[test]
    fn blank_descriptions_are_omitted_rather_than_emptied() {
        let schema = build(&[SpotlightAiToolParam {
            description: Some("   ".to_string()),
            ..param("value", AiParamType::String, false)
        }]);

        assert!(schema["properties"]["value"].get("description").is_none());
    }

    #[test]
    fn required_lists_only_the_required_params_in_order() {
        let schema = build(&[
            param("a", AiParamType::String, true),
            param("b", AiParamType::String, false),
            param("c", AiParamType::String, true),
        ]);

        assert_eq!(schema["required"], serde_json::json!(["a", "c"]));
    }
}
