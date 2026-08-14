use serde_json::Value;

pub fn decode_result(output: &[u8]) -> Result<Value, String> {
    let response: Value = serde_json::from_slice(output)
        .map_err(|error| format!("Claude CLI returned invalid JSON: {error}"))?;
    match response {
        Value::Object(_) => Ok(response),
        Value::Array(events) => events
            .into_iter()
            .rev()
            .find(|event| event.get("type").and_then(Value::as_str) == Some("result"))
            .ok_or_else(|| "Claude CLI event stream contained no result".to_string()),
        _ => Err("Claude CLI returned an unexpected JSON envelope".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_single_result_object() {
        let result = decode_result(br#"{"type":"result","session_id":"one"}"#).unwrap();
        assert_eq!(result["session_id"], "one");
    }

    #[test]
    fn extracts_result_from_event_array() {
        let result = decode_result(
            br#"[
                {"type":"system","subtype":"init"},
                {"type":"result","session_id":"two"}
            ]"#,
        )
        .unwrap();
        assert_eq!(result["session_id"], "two");
    }
}
