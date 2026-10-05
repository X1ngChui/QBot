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
