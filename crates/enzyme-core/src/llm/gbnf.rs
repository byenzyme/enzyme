//! Minimal JSON-Schema → GBNF conversion for grammar-constrained local decoding.
//!
//! Enzyme's pipeline already describes catalyst responses as `response_format:
//! json_schema`, so the local engine converts that schema rather than carrying a
//! second hand-written grammar that could drift from it. The converter covers
//! exactly the subset the catalyst schemas use — object / array / string /
//! number / integer / boolean — and errors on anything else instead of emitting
//! a permissive grammar (llama.cpp's own converter is more general but has open
//! correctness bugs, and a silently loose grammar is worse than a hard failure
//! for the 135M checkpoint, which is 95% nonwords unconstrained).
//!
//! Two deliberate restrictions:
//! - only `required` properties are emitted, in schema order, so the grammar is
//!   a single deterministic key sequence (every catalyst schema sets
//!   `additionalProperties: false` and requires all its properties);
//! - arrays and strings are length-capped, because GBNF guarantees *prefix*
//!   validity only — an unbounded array that runs into the token cap yields
//!   unparseable JSON.

use anyhow::{Result, bail};
use serde_json::Value;

/// Maximum items in any generated array.
const MAX_ARRAY_ITEMS: usize = 24;
/// Maximum characters in any generated string.
const MAX_STRING_CHARS: usize = 400;

/// Convert a JSON Schema into a GBNF grammar whose `root` matches it.
pub fn json_schema_to_gbnf(schema: &Value) -> Result<String> {
    let mut rules: Vec<String> = Vec::new();
    let root = convert(schema, "root", &mut rules)?;
    if root != "root" {
        rules.push(format!("root ::= {root}"));
    }
    rules.push(r#"ws ::= [ \t\n]{0,2}"#.to_string());
    rules.push(
        r#"string ::= "\"" ([^"\\\x00-\x1F] | "\\" (["\\/bfnrt] | "u" [0-9a-fA-F]{4})){0,400} "\"""#
            .replace("{0,400}", &format!("{{0,{MAX_STRING_CHARS}}}")),
    );
    rules.push(r#"number ::= "-"? ("0" | [1-9] [0-9]{0,15}) ("." [0-9]{1,16})?"#.to_string());
    rules.push(r#"integer ::= "-"? ("0" | [1-9] [0-9]{0,15})"#.to_string());
    rules.push(r#"boolean ::= "true" | "false""#.to_string());
    Ok(rules.join("\n"))
}

/// Emit rules for `schema` and return the name of the rule matching it.
fn convert(schema: &Value, name: &str, rules: &mut Vec<String>) -> Result<String> {
    let ty = schema
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("schema node '{name}' has no `type`"))?;

    match ty {
        "string" => Ok("string".to_string()),
        "number" => Ok("number".to_string()),
        "integer" => Ok("integer".to_string()),
        "boolean" => Ok("boolean".to_string()),
        "array" => {
            let items = schema
                .get("items")
                .ok_or_else(|| anyhow::anyhow!("array '{name}' has no `items`"))?;
            let item_rule = convert(items, &format!("{name}-item"), rules)?;
            let tail = MAX_ARRAY_ITEMS.saturating_sub(1);
            rules.push(format!(
                r#"{name} ::= "[" ws ({item_rule} (ws "," ws {item_rule}){{0,{tail}}})? ws "]""#
            ));
            Ok(name.to_string())
        }
        "object" => {
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow::anyhow!("object '{name}' has no `properties`"))?;
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if required.is_empty() {
                bail!("object '{name}' has no `required` properties — unsupported");
            }

            let mut parts: Vec<String> = Vec::new();
            for (i, key) in required.iter().enumerate() {
                let prop = properties
                    .get(*key)
                    .ok_or_else(|| anyhow::anyhow!("required key '{key}' missing from properties"))?;
                let rule = convert(prop, &format!("{name}-{key}"), rules)?;
                let sep = if i == 0 { "" } else { r#""," ws "# };
                parts.push(format!(r#"{sep}"\"{key}\"" ws ":" ws {rule} ws"#));
            }
            rules.push(format!(r#"{name} ::= "{{" ws {} "}}""#, parts.join(" ")));
            Ok(name.to_string())
        }
        other => bail!("unsupported JSON Schema type '{other}' in '{name}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{catalyst_entries_json_schema, catalyst_json_schema};

    #[test]
    fn converts_the_catalyst_schema() {
        let gbnf = json_schema_to_gbnf(&catalyst_json_schema(false)).unwrap();
        assert!(gbnf.contains(r#"root ::= "{" ws "\"catalysts\"""#), "{gbnf}");
        assert!(gbnf.contains(r#""\"topicNames\"""#), "{gbnf}");
        assert!(gbnf.contains("ws ::="));
        assert!(gbnf.contains("string ::="));
    }

    #[test]
    fn converts_the_streaming_entries_schema() {
        let gbnf = json_schema_to_gbnf(&catalyst_entries_json_schema(true)).unwrap();
        assert!(gbnf.contains(r#""\"entries\"""#), "{gbnf}");
        assert!(gbnf.contains(r#""\"eraLabel\"""#), "{gbnf}");
    }

    #[test]
    fn arrays_are_length_capped() {
        let gbnf = json_schema_to_gbnf(&catalyst_json_schema(false)).unwrap();
        assert!(gbnf.contains(&format!("{{0,{}}}", MAX_ARRAY_ITEMS - 1)), "{gbnf}");
    }

    #[test]
    fn rejects_unsupported_types() {
        let schema = serde_json::json!({ "type": "null" });
        assert!(json_schema_to_gbnf(&schema).is_err());
    }

    #[test]
    fn rejects_untyped_nodes() {
        assert!(json_schema_to_gbnf(&serde_json::json!({})).is_err());
    }
}
