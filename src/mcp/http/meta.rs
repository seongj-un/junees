//! Request metadata the modern Streamable HTTP binding mirrors into
//! headers (MCP 2026-07-28, "Request Metadata"): `Mcp-Method`,
//! `Mcp-Name`, and `Mcp-Param-{Name}` for tool parameters a server marks
//! with `x-mcp-header`, with the Base64 sentinel for values that are not
//! plain header text.

use serde_json::{Map, Value};

/// The largest integer a parameter header may carry (JavaScript's safe
/// range).
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// The `Mcp-Name` value of a request: `params.name` of `tools/call` and
/// `prompts/get`, `params.uri` of `resources/read`.
pub fn name_header(method: &str, params: &Value) -> Option<String> {
    let key = match method {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        _ => return None,
    };
    params.get(key).and_then(Value::as_str).map(encode)
}

/// `text` as a header value: as it is when it is plain visible ASCII
/// (spaces and tabs inside), otherwise `=?base64?<UTF-8 in Base64>?=`.
/// Plain text that looks like the sentinel is encoded too.
pub fn encode(text: &str) -> String {
    let plain = text
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        && !text.starts_with([' ', '\t'])
        && !text.ends_with([' ', '\t'])
        && !(text.starts_with("=?base64?") && text.ends_with("?="));
    if plain {
        text.to_string()
    } else {
        format!("=?base64?{}?=", base64(text.as_bytes()))
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A tool parameter mirrored into a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamHeader {
    /// The chain of `properties` keys from the schema root.
    pub path: Vec<String>,
    /// The `{Name}` of `Mcp-Param-{Name}`.
    pub name: String,
}

/// The `x-mcp-header` annotations of a tool's input schema, or why the
/// tool must be left out (the binding says a client rejects such a tool):
/// an empty or non-token name, two names equal ignoring case, a
/// non-primitive or `number` parameter, or an annotation not reachable
/// from the root through `properties` alone.
pub fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut found = Vec::new();
    walk(schema, Some(&mut Vec::new()), true, &mut found)?;
    for (i, header) in found.iter().enumerate() {
        if found[..i]
            .iter()
            .any(|other| other.name.eq_ignore_ascii_case(&header.name))
        {
            return Err(format!(
                "two parameters use the header name `{}`",
                crate::prompt_text::escape_hidden(&header.name)
            ));
        }
    }
    Ok(found)
}

/// Schema keywords whose values are data, not schemas.
const DATA_KEYWORDS: &[&str] = &["const", "default", "enum", "examples"];

/// Walks `node`; `path` is `Some` while the node is reached through
/// `properties` keys only.
fn walk(
    node: &Value,
    path: Option<&mut Vec<String>>,
    root: bool,
    found: &mut Vec<ParamHeader>,
) -> Result<(), String> {
    match node {
        Value::Array(items) => {
            for item in items {
                walk(item, None, false, found)?;
            }
            Ok(())
        }
        Value::Object(object) => walk_object(object, path, root, found),
        _ => Ok(()),
    }
}

fn walk_object(
    object: &Map<String, Value>,
    mut path: Option<&mut Vec<String>>,
    root: bool,
    found: &mut Vec<ParamHeader>,
) -> Result<(), String> {
    if let Some(annotation) = object.get("x-mcp-header") {
        let name = annotation.as_str().unwrap_or_default();
        let shown = || crate::prompt_text::escape_hidden(name);
        let Some(at) = path.as_deref().filter(|_| !root) else {
            return Err(format!(
                "its `x-mcp-header` `{}` is not on a parameter reached through `properties` \
                 alone",
                shown()
            ));
        };
        if name.is_empty() || !name.bytes().all(is_tchar) {
            return Err(format!(
                "its `x-mcp-header` `{}` is not a valid header name",
                shown()
            ));
        }
        if !matches!(
            object.get("type").and_then(Value::as_str),
            Some("string" | "integer" | "boolean")
        ) {
            return Err(format!(
                "its `x-mcp-header` `{}` is on a parameter that is not a string, integer or \
                 boolean",
                shown()
            ));
        }
        found.push(ParamHeader {
            path: at.clone(),
            name: name.to_string(),
        });
    }
    for (key, value) in object {
        if DATA_KEYWORDS.contains(&key.as_str()) {
            continue;
        }
        if key == "properties"
            && let (Value::Object(properties), Some(path)) = (value, path.as_deref_mut())
        {
            for (property, schema) in properties {
                path.push(property.clone());
                let result = walk(schema, Some(path), false, found);
                path.pop();
                result?;
            }
            continue;
        }
        if let Value::Object(properties) = value
            && key == "properties"
        {
            for schema in properties.values() {
                walk(schema, None, false, found)?;
            }
            continue;
        }
        walk(value, None, false, found)?;
    }
    Ok(())
}

/// RFC 9110 `tchar`.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The `Mcp-Param-{Name}` headers of a call: the value at each path, as
/// text, encoded; absent and `null` values (and values of the wrong type)
/// give no header.
pub fn param_values(headers: &[ParamHeader], arguments: &Value) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|header| {
            let value = header
                .path
                .iter()
                .try_fold(arguments, |value, key| value.get(key))?;
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Bool(flag) => flag.to_string(),
                Value::Number(number) => number
                    .as_i64()
                    .filter(|n| n.abs() <= MAX_SAFE_INTEGER)?
                    .to_string(),
                _ => return None,
            };
            Some((format!("Mcp-Param-{}", header.name), encode(&text)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_are_encoded_as_the_binding_shows() {
        // The table of "Value Encoding".
        assert_eq!(encode("us-west1"), "us-west1");
        assert_eq!(encode("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(encode(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(encode("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(
            encode("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
        assert_eq!(encode("a b\tc"), "a b\tc");
        assert_eq!(encode(""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd, 0xfc]), "//79/A==");
    }

    #[test]
    fn names_come_from_name_or_uri() {
        let params = json!({"name": "get_weather", "uri": "file:///a b"});
        assert_eq!(
            name_header("tools/call", &params).as_deref(),
            Some("get_weather")
        );
        assert_eq!(
            name_header("prompts/get", &params).as_deref(),
            Some("get_weather")
        );
        assert_eq!(
            name_header("resources/read", &params).as_deref(),
            Some("file:///a b")
        );
        assert_eq!(name_header("tools/list", &params), None);
        assert_eq!(
            name_header("tools/call", &json!({"name": "날씨"})).as_deref(),
            Some("=?base64?64Kg7JSo?=")
        );
    }

    #[test]
    fn annotated_parameters_become_headers() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "query": {"type": "string"},
                "limit": {"type": "integer", "x-mcp-header": "Limit"},
                "options": {"type": "object", "properties": {
                    "dry": {"type": "boolean", "x-mcp-header": "Dry-Run"},
                    "x-mcp-header": {"type": "string"}
                }},
                "fallback": {"type": "string", "default": {"x-mcp-header": "data"}}
            }
        });
        let headers = param_headers(&schema).unwrap();
        assert_eq!(headers.len(), 3);
        let values = param_values(
            &headers,
            &json!({"region": "서울", "limit": 42, "options": {"dry": false}, "query": "q"}),
        );
        assert_eq!(
            values,
            [
                ("Mcp-Param-Limit".to_string(), "42".to_string()),
                ("Mcp-Param-Dry-Run".to_string(), "false".to_string()),
                (
                    "Mcp-Param-Region".to_string(),
                    "=?base64?7ISc7Jq4?=".to_string()
                ),
            ]
        );
        // Missing, null and mistyped values give no header.
        assert!(
            param_values(
                &headers,
                &json!({"region": null, "limit": 1.5, "options": 3})
            )
            .is_empty()
        );
        assert!(param_values(&headers, &json!({"limit": 1i64 << 53})).is_empty());
    }

    #[test]
    fn invalid_annotations_reject_the_tool() {
        let reject = |schema: Value| param_headers(&schema).unwrap_err();
        let prop = |p: Value| json!({"type": "object", "properties": {"p": p}});
        assert!(reject(prop(json!({"type": "string", "x-mcp-header": ""}))).contains("valid"));
        assert!(reject(prop(json!({"type": "string", "x-mcp-header": "a b"}))).contains("valid"));
        assert!(
            reject(prop(json!({"type": "string", "x-mcp-header": "a\r\nX: 1"}))).contains("valid")
        );
        assert!(
            reject(prop(json!({"type": "number", "x-mcp-header": "N"}))).contains("not a string")
        );
        assert!(reject(prop(json!({"x-mcp-header": "N"}))).contains("not a string"));
        assert!(
            reject(prop(
                json!({"type": "array", "items": {"type": "string", "x-mcp-header": "I"}})
            ))
            .contains("not on a parameter")
        );
        assert!(
            reject(prop(
                json!({"anyOf": [{"type": "string", "x-mcp-header": "A"}]})
            ))
            .contains("not on a parameter")
        );
        assert!(
            reject(json!({"type": "object", "x-mcp-header": "Root"}))
                .contains("not on a parameter")
        );
        assert!(
            reject(json!({"$defs": {"d": {"type": "object", "properties": {
                "p": {"type": "string", "x-mcp-header": "D"}}}}}))
            .contains("not on a parameter")
        );
        assert!(
            reject(json!({"type": "object", "properties": {
                "a": {"type": "string", "x-mcp-header": "Same"},
                "b": {"type": "string", "x-mcp-header": "sAME"}
            }}))
            .contains("two parameters")
        );
        assert!(
            param_headers(&json!({"type": "object"}))
                .unwrap()
                .is_empty()
        );
    }
}
