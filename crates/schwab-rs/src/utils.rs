//! Helpers shared by the digest, audit and reconciliation paths.

/// Stable object-key ordering regardless of serde_json's preserve_order feature.
/// Arrays and scalar representations remain unchanged.
///
/// Intent digests, audit-delivery bodies and the paper journal hash the
/// result, so two values that differ only in key order must canonicalize to
/// the same bytes.
pub fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&map[key]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_at_every_depth_and_keeps_array_order() {
        let a = json!({"z": 1, "a": {"y": [{"q": 1, "b": 2}, 3], "x": null}});
        let b = json!({"a": {"x": null, "y": [{"b": 2, "q": 1}, 3]}, "z": 1});
        let canonical = serde_json::to_string(&canonical_json(&a)).unwrap();
        assert_eq!(
            canonical,
            serde_json::to_string(&canonical_json(&b)).unwrap()
        );
        assert_eq!(canonical, r#"{"a":{"x":null,"y":[{"b":2,"q":1},3]},"z":1}"#);

        let reordered = json!([2, 1]);
        assert_eq!(canonical_json(&reordered), reordered);
    }
}
