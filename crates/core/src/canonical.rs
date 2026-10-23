//! Canonical JSON (sorted keys, no whitespace) and the hashes taken over it (10.3).

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The canonical serialisation: object keys sorted, no whitespace.
///
/// `serde_json::Value` keeps objects in a `BTreeMap` (the `preserve_order` feature is not
/// enabled anywhere in this workspace), so serialising a `Value` sorts keys by byte order.
pub fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).expect("a serde_json::Value always serialises")
}

/// sha256 of a string, as lowercase hex.
pub fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// sha256 of a value's canonical JSON.
pub fn hash_value(value: &Value) -> String {
    sha256_hex(&canonical_json(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_and_whitespace_dropped_at_every_depth() {
        let v: Value = serde_json::from_str(r#"{ "b": 1, "a": { "z": [1, 2], "c": null } }"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":{"c":null,"z":[1,2]},"b":1}"#);
    }

    #[test]
    fn hash_does_not_depend_on_key_order() {
        let a: Value = serde_json::from_str(r#"{"x":1,"y":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":2,"x":1}"#).unwrap();
        assert_eq!(hash_value(&a), hash_value(&b));
        assert_ne!(hash_value(&a), hash_value(&json!({"x": 1, "y": 3})));
    }
}
