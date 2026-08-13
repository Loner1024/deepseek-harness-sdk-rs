//! Structural extraction over the opaque session-event stream.
//!
//! Event payloads ride the wire as JSON and pass through the SDK verbatim;
//! this module applies structural reads at exactly the three points the
//! Python SDK parses: the inbox receipt, the last committed assistant text,
//! and the last `turn/end` reason kind.

use serde_json::Value;

use crate::client::Notification;
use crate::error::SdkError;

/// Whether `notification` is the `agent/inbox/spliced` receipt proving that
/// `message_id` was durably enqueued on `session_id`.
pub(crate) fn is_inbox_receipt(
    notification: &Notification,
    session_id: &str,
    message_id: &str,
) -> bool {
    if notification.method != "session.event" {
        return false;
    }
    if notification
        .payload
        .get("sessionId")
        .and_then(Value::as_str)
        != Some(session_id)
    {
        return false;
    }
    let Some(event) = notification.payload.get("event") else {
        return false;
    };
    if event.get("type").and_then(Value::as_str) != Some("agent/inbox/spliced") {
        return false;
    }
    let Some(inserted) = event.get("data").and_then(|d| d.get("inserted")) else {
        return false;
    };
    inserted.as_array().is_some_and(|messages| {
        messages
            .iter()
            .any(|message| message.get("id").and_then(Value::as_str) == Some(message_id))
    })
}

/// The text blocks of the last committed `assistant/message` event,
/// concatenated; empty when no such event exists.
pub(crate) fn final_response(events: &[Value]) -> String {
    for event in events.iter().rev() {
        if event.get("type").and_then(Value::as_str) != Some("assistant/message") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        // The message content lives on `data.message` when present and on
        // `data` itself otherwise, mirroring the Python SDK's content owner.
        let content_owner = data
            .get("message")
            .filter(|m| m.is_object())
            .unwrap_or(data);
        let Some(content) = content_owner.get("content") else {
            continue;
        };
        let Some(blocks) = content.as_array() else {
            continue;
        };
        let mut parts = String::new();
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                parts.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""));
            }
        }
        return parts;
    }
    String::new()
}

/// The reason kind of the last `turn/end` event; `None` when the collected
/// events contain no turn end. A `turn/end` without a string
/// `data.reason.kind` is a documented-protocol violation.
pub(crate) fn finish_reason(events: &[Value]) -> Result<Option<String>, SdkError> {
    for event in events.iter().rev() {
        if event.get("type").and_then(Value::as_str) != Some("turn/end") {
            continue;
        }
        let kind = event
            .get("data")
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str);
        let Some(kind) = kind else {
            return Err(SdkError::protocol(
                "turn/end event requires a string data.reason.kind",
            ));
        };
        return Ok(Some(kind.to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbox_receipt_matches_only_the_receipted_message() {
        let notification = Notification {
            method: "session.event".into(),
            payload: json!({
                "sessionId": "s1",
                "event": {"type": "agent/inbox/spliced", "data": {"inserted": [{"id": "m2"}]}}
            }),
        };
        assert!(is_inbox_receipt(&notification, "s1", "m2"));
        assert!(!is_inbox_receipt(&notification, "s1", "m1"));
        assert!(!is_inbox_receipt(&notification, "other", "m2"));
    }

    #[test]
    fn inbox_receipt_rejects_other_notifications_and_shapes() {
        let notification = Notification {
            method: "session.status".into(),
            payload: json!({"sessionId": "s1", "status": "idle"}),
        };
        assert!(!is_inbox_receipt(&notification, "s1", "m2"));
        let malformed = Notification {
            method: "session.event".into(),
            payload: json!({"sessionId": "s1", "event": {"type": "assistant/message", "data": {}}}),
        };
        assert!(!is_inbox_receipt(&malformed, "s1", "m2"));
    }

    #[test]
    fn final_response_takes_the_last_assistant_text() {
        let events = vec![
            json!({"type": "assistant/message", "data": {"message": {"content": [{"type": "text", "text": "first"}]}}}),
            json!({"type": "assistant/message", "data": {"content": [{"type": "text", "text": "second"}, {"type": "tool_use", "id": "t"}]}}),
        ];
        assert_eq!(final_response(&events), "second");
    }

    #[test]
    fn final_response_is_empty_without_assistant_events() {
        assert_eq!(final_response(&[]), "");
        assert_eq!(
            final_response(&[json!({"type": "turn/end", "data": {"reason": {"kind": "ok"}}})]),
            ""
        );
    }

    #[test]
    fn finish_reason_reads_the_last_turn_end() {
        let events = vec![
            json!({"type": "turn/end", "data": {"reason": {"kind": "stop"}}}),
            json!({"type": "turn/end", "data": {"reason": {"kind": "max-tokens"}}}),
        ];
        assert_eq!(finish_reason(&events).unwrap(), Some("max-tokens".into()));
        assert_eq!(finish_reason(&[]).unwrap(), None);
    }

    #[test]
    fn finish_reason_rejects_a_missing_kind() {
        let events = vec![json!({"type": "turn/end", "data": {"reason": null}})];
        assert!(matches!(
            finish_reason(&events),
            Err(SdkError::Protocol { .. })
        ));
    }
}
