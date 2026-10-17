//! The MCP tool reference, `docs/tools.md`, made from the server's own tool
//! list so that it always says what the server offers.

use std::collections::BTreeMap;
use std::fmt::Write;

use rmcp::model::Tool;
use serde_json::Value;

/// What `docs/tools.md` starts with.
const HEADER: &str = "\
# treason's MCP tools

<!-- Made by `cargo test -p treason-mcp` from the server's tool list. Don't edit it by hand. -->

Every tool that `treason serve` offers over MCP: what it does, what it
takes and what it returns. This file is made from the server's own tool
list, so it matches the code. After changing a tool, update it with
`TREASON_UPDATE_DOCS=1 cargo test -p treason-mcp`. [guide.md](guide.md)
explains how the tools fit together, with examples.
";

/// The reference for `tools`, in Markdown.
pub(crate) fn render(tools: &[Tool]) -> String {
    let mut tools: Vec<&Tool> = tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let types = types(&tools);
    let mut out = String::from(HEADER);

    out.push_str("\n| Tool | What it does |\n|---|---|\n");
    for tool in &tools {
        let summary = first_sentence(tool.description.as_deref().unwrap_or_default());
        line(
            &mut out,
            format_args!("| [`{0}`](#{0}) | {1} |", tool.name, cell(summary)),
        );
    }

    // Results that aren't a named type, by the first tool that returns each.
    let mut results: Vec<(Value, &str)> = Vec::new();
    for tool in &tools {
        line(&mut out, format_args!("\n## {}\n", tool.name));
        line(
            &mut out,
            format_args!("{}", prose(tool.description.as_deref().unwrap_or_default())),
        );
        if tool.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true) {
            out.push_str("\nRead only: it changes nothing.\n");
        }

        let input = Value::Object((*tool.input_schema).clone());
        if has_fields(&input) {
            out.push_str("\nInput:\n\n");
            fields(&mut out, &input);
        } else {
            out.push_str("\nNo input: call it with `{}`.\n");
        }

        let Some(schema) = &tool.output_schema else {
            out.push_str("\nReturns text.\n");
            continue;
        };
        let result = bare(&Value::Object((**schema).clone()));
        if let Some(name) = types
            .iter()
            .find_map(|(name, schema)| (bare(schema) == result).then_some(name))
        {
            line(&mut out, format_args!("\nReturns {}.", link(name)));
        } else if let Some((_, first)) = results.iter().find(|(seen, _)| *seen == result) {
            line(
                &mut out,
                format_args!("\nReturns the same as [`{first}`](#{first})."),
            );
        } else {
            out.push_str("\nReturns:\n\n");
            fields(&mut out, &result);
            results.push((result, &tool.name));
        }
    }

    out.push_str("\n## Types\n\nThe types that the tools above take and return.\n");
    for (name, schema) in &types {
        line(&mut out, format_args!("\n### {name}\n"));
        if let Some(text) = schema.get("description").and_then(Value::as_str) {
            line(&mut out, format_args!("{}\n", prose(text)));
        }
        if has_fields(schema) {
            fields(&mut out, schema);
        } else if let Some(variants) = variants(schema) {
            out.push_str("One of:\n\n");
            for variant in variants {
                let text = variant
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                line(
                    &mut out,
                    format_args!("- {}: {}", type_of(variant), cell(text)),
                );
            }
        } else {
            line(&mut out, format_args!("A {}.", type_of(schema)));
        }
    }
    out
}

/// Every named type in the tools' schemas. A name that two schemas define
/// differently is a bug in the reference, so it panics.
fn types(tools: &[&Tool]) -> BTreeMap<String, Value> {
    let mut types = BTreeMap::new();
    let schemas = tools
        .iter()
        .flat_map(|tool| std::iter::once(&tool.input_schema).chain(tool.output_schema.as_ref()));
    for schema in schemas {
        let Some(Value::Object(defs)) = schema.get("$defs") else {
            continue;
        };
        for (name, definition) in defs {
            let known = types
                .entry(name.clone())
                .or_insert_with(|| definition.clone());
            assert_eq!(known, definition, "two schemas define `{name}` differently");
        }
    }
    types
}

/// A schema's structure, without its description or what only a root
/// schema has, to compare a tool's result with the named types.
fn bare(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Value::Object(map) = &mut schema {
        for key in ["$schema", "$defs", "title", "description"] {
            map.remove(key);
        }
    }
    schema
}

fn has_fields(schema: &Value) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| !properties.is_empty())
}

/// A table of an object's fields, the required ones first.
fn fields(out: &mut String, schema: &Value) {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut properties: Vec<(&String, &Value)> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.iter().collect())
        .unwrap_or_default();
    properties.sort_by_key(|(name, _)| (!required.contains(&name.as_str()), name.as_str()));
    out.push_str("| Field | Type | Description |\n|---|---|---|\n");
    for (name, property) in properties {
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            ", optional"
        };
        let text = property
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        line(
            out,
            format_args!(
                "| `{name}` | {}{optional} | {} |",
                type_of(property),
                cell(text)
            ),
        );
    }
}

/// The alternatives of an enum or an untagged union, leaving out null.
fn variants(schema: &Value) -> Option<Vec<&Value>> {
    ["oneOf", "anyOf"].iter().find_map(|key| {
        let variants = schema.get(*key)?.as_array()?;
        Some(
            variants
                .iter()
                .filter(|variant| !is_null(variant))
                .collect(),
        )
    })
}

fn is_null(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
}

/// A field's type in a few words, linking to named types.
fn type_of(schema: &Value) -> String {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return link(reference.trim_start_matches("#/$defs/"));
    }
    if let Some(value) = schema.get("const") {
        return format!("`{value}`");
    }
    if let Some(variants) = variants(schema) {
        return variants
            .into_iter()
            .map(type_of)
            .collect::<Vec<_>>()
            .join(" or ");
    }
    let kind = match schema.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|&kind| kind != "null")
            .unwrap_or("null"),
        _ => "any value",
    };
    match kind {
        "array" => match schema.get("items") {
            Some(items) => format!("list of {}", type_of(items)),
            None => "list".into(),
        },
        "object" => match schema.get("additionalProperties") {
            Some(values @ Value::Object(_)) => format!("map of names to {}", type_of(values)),
            _ => "object".into(),
        },
        kind => kind.into(),
    }
}

fn link(name: &str) -> String {
    format!("[`{name}`](#{})", name.to_lowercase())
}

/// Doc-comment text as Markdown paragraphs: lines joined within each.
fn prose(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Text for a table cell: one line, with its pipes escaped.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// The first sentence of a description.
fn first_sentence(text: &str) -> &str {
    text.find(". ").map_or(text, |end| &text[..=end])
}

fn line(out: &mut String, text: std::fmt::Arguments<'_>) {
    out.write_fmt(text).expect("writing to a String can't fail");
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn types_read_in_a_few_words() {
        let cases = [
            (json!({"type": "string"}), "string"),
            (json!({"type": ["number", "null"]}), "number"),
            (
                json!({"$ref": "#/$defs/NoteSpec"}),
                "[`NoteSpec`](#notespec)",
            ),
            (
                json!({"anyOf": [{"$ref": "#/$defs/Ramp"}, {"type": "null"}]}),
                "[`Ramp`](#ramp)",
            ),
            (
                json!({"type": "array", "items": {"type": "integer"}}),
                "list of integer",
            ),
            (
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                "map of names to string",
            ),
            (
                json!({"oneOf": [{"const": "linear"}, {"const": "smooth"}]}),
                "`\"linear\"` or `\"smooth\"`",
            ),
        ];
        for (schema, expected) in cases {
            assert_eq!(type_of(&schema), expected, "{schema}");
        }
    }

    #[test]
    fn descriptions_become_paragraphs_and_cells() {
        let text = "One line\nand the next.\n\nA second | paragraph.";
        assert_eq!(
            prose(text),
            "One line and the next.\n\nA second | paragraph."
        );
        assert_eq!(cell(text), "One line and the next. A second \\| paragraph.");
        assert_eq!(first_sentence("Adds a track. Then more."), "Adds a track.");
        assert_eq!(first_sentence("No full stop"), "No full stop");
    }
}
