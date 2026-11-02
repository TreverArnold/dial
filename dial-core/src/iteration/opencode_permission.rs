//! Recognize permission failures from tool state, never from quoted model output.
use serde_json::Value;

pub(super) fn rejected_tool(part: &Value) -> Option<String> {
    if part.get("type")?.as_str()? != "tool" {
        return None;
    }
    let state = part.get("state")?;
    if state.get("status")?.as_str()? != "error" {
        return None;
    }
    let error = state.get("error")?.as_str()?.to_ascii_lowercase();
    if !error.contains("permission") || !(error.contains("rejected") || error.contains("denied")) {
        return None;
    }
    let tool = part
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("unknown tool");
    let command = state.pointer("/input/command").and_then(Value::as_str);
    // Bound diagnostics and remove terminal control characters.
    let detail: String = command
        .unwrap_or(tool)
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
    Some(format!(
        "OpenCode permission rejected for {tool}: {detail}. Operator review required; no retry."
    ))
}

pub(super) fn rejected_message(messages: &Value) -> Option<String> {
    messages.as_array()?.iter().find_map(|message| {
        message
            .get("parts")?
            .as_array()?
            .iter()
            .find_map(rejected_tool)
    })
}

pub(super) fn rejected_event(line: &str) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    rejected_tool(event.get("part")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rejection() -> Value {
        json!({"type":"tool","tool":"bash","state":{
            "status":"error","input":{"command":"cd /tmp && node /repo/verify.mjs --list"},
            "error":"The user rejected permission to use this specific tool call."}})
    }

    #[test]
    fn extracts_command_from_stream_and_session_errors() {
        let part = rejection();
        let reason = rejected_event(&json!({"type":"tool_use","part":part}).to_string()).unwrap();
        assert!(reason.contains("cd /tmp && node /repo/verify.mjs --list"));
        assert_eq!(
            rejected_message(&json!([{"parts":[rejection()]}])),
            Some(reason)
        );
    }

    #[test]
    fn ignores_quoted_errors_and_ordinary_test_failures() {
        assert!(rejected_event("permission denied").is_none());
        assert!(rejected_event(
            &json!({"part":{"type":"text","text":"permission denied"}}).to_string()
        )
        .is_none());
        let mut part = rejection();
        part["state"]["status"] = json!("completed");
        assert!(rejected_tool(&part).is_none());
        part["state"]["status"] = json!("error");
        part["state"]["error"] = json!("Test failed: expected 200, got 400");
        assert!(rejected_tool(&part).is_none());
    }

    #[test]
    fn bounds_command_diagnostics() {
        let mut part = rejection();
        part["state"]["input"]["command"] = json!("x".repeat(2000));
        assert!(rejected_tool(&part).unwrap().len() < 650);
    }
}
