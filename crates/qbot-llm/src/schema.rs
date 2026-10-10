//! Tool argument schemas as models receive them.

use schemars::JsonSchema;
use serde_json::Value;

/// The self-contained JSON schema of `T` (nested types inlined, no `$schema` or `title`), with
/// the given descriptions set on its fields. A field path is `a` or `a.b`, through arrays, so the
/// wording of parameters can live apart from the argument types.
pub fn tool_schema<'a, T: JsonSchema>(
    descriptions: impl IntoIterator<Item = (&'a str, String)>,
) -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator();
    let mut schema =
        serde_json::to_value(generator.into_root_schema_for::<T>()).unwrap_or(Value::Null);
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    for (path, description) in descriptions {
        describe_field(&mut schema, path, description);
    }
    schema
}

/// Set `description` on the schema of the field at `path`.
fn describe_field(schema: &mut Value, path: &str, description: String) {
    let mut node = schema;
    for part in path.split('.') {
        // Arrays describe their items' fields.
        while node.get("items").is_some() {
            let Some(items) = node.get_mut("items") else {
                return;
            };
            node = items;
        }
        let Some(field) = node.get_mut("properties").and_then(|p| p.get_mut(part)) else {
            return;
        };
        node = field;
    }
    if let Some(object) = node.as_object_mut() {
        object.insert("description".into(), Value::String(description));
    }
}

/// The fields of `schema` (paths as in [`tool_schema`]) that have no description: a model sees
/// a field's name and type, and only the description says what it is for.
pub fn undescribed(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_undescribed(schema, "", &mut out);
    out
}

fn collect_undescribed(node: &Value, prefix: &str, out: &mut Vec<String>) {
    let mut node = node;
    while let Some(items) = node.get("items") {
        node = items;
    }
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, field) in properties {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if field.get("description").is_none() {
            out.push(path.clone());
        }
        collect_undescribed(field, &path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::{tool_schema, undescribed};
    use schemars::JsonSchema;

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        query: String,
        items: Vec<Item>,
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Item {
        id: u32,
    }

    #[test]
    fn a_field_left_without_description_is_found_through_arrays() {
        let full = tool_schema::<Args>([
            ("query", "q".to_owned()),
            ("items", "i".to_owned()),
            ("items.id", "id".to_owned()),
        ]);
        assert!(undescribed(&full).is_empty(), "{full}");
        // A mistyped path describes nothing, so its field shows up as undescribed.
        let typo = tool_schema::<Args>([
            ("query", "q".to_owned()),
            ("items", "i".to_owned()),
            ("item.id", "id".to_owned()),
        ]);
        assert_eq!(undescribed(&typo), ["items.id"]);
    }
}
