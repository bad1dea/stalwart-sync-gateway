use anyhow::{bail, Context};
use serde_json::{json, Map, Value};

fn display_alerts(alerts: &Value) -> anyhow::Result<Vec<(String, &Value)>> {
    let Some(alerts) = alerts.as_object() else {
        if alerts.is_null() {
            return Ok(Vec::new());
        }
        bail!("invalid task alerts");
    };
    Ok(alerts
        .iter()
        .filter(|(_, alert)| {
            alert
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("display")
                == "display"
        })
        .map(|(id, alert)| (id.clone(), alert))
        .collect())
}

pub fn read_reminder(alerts: &Value) -> Option<String> {
    let mut display = display_alerts(alerts).ok()?;
    if display.len() != 1 {
        return None;
    }
    let (_, alert) = display.pop()?;
    if alert["trigger"]["@type"] != "AbsoluteTrigger" {
        return None;
    }
    let value = alert["trigger"]["when"].as_str()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(parsed.format("%Y-%m-%dT%H:%M:%S.000Z").to_string())
}

/// Applies an EAS absolute task reminder while retaining unrelated JMAP alerts.
/// The gateway cannot safely edit the effective alert set when calendar defaults
/// are active, or when the task has multiple display alerts.
pub fn write_reminder(
    task: &mut Map<String, Value>,
    alerts: &Value,
    use_default_alerts: bool,
    reminder_set: Option<bool>,
    reminder_time: Option<&str>,
) -> anyhow::Result<()> {
    if reminder_set.is_none() && reminder_time.is_none() {
        return Ok(());
    }
    if use_default_alerts {
        bail!("task uses calendar default alerts");
    }
    let mut displays = display_alerts(alerts)?;
    if displays.len() > 1 {
        bail!("task has multiple display alerts");
    }
    let enabled = reminder_set
        .or_else(|| reminder_time.map(|_| true))
        .unwrap_or(false);
    if !enabled {
        if reminder_time.is_some() {
            bail!("task reminder time conflicts with ReminderSet=0");
        }
        if let Some((id, _)) = displays.pop() {
            let mut remaining = alerts.as_object().cloned().unwrap_or_default();
            remaining.remove(&id);
            task.insert("alerts".to_owned(), Value::Object(remaining));
            task.insert("useDefaultAlerts".to_owned(), json!(false));
        }
        return Ok(());
    }
    let when = reminder_time
        .or_else(|| {
            displays
                .first()
                .and_then(|(_, alert)| alert["trigger"]["when"].as_str())
        })
        .context("ReminderTime required when enabling a task reminder")?;
    let parsed = chrono::DateTime::parse_from_rfc3339(when)
        .context("invalid task reminder time")?
        .with_timezone(&chrono::Utc);
    let normalized = parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let id = displays
        .first()
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| format!("eas{}", uuid::Uuid::new_v4().simple()));
    let mut remaining = alerts.as_object().cloned().unwrap_or_default();
    let mut alert = remaining
        .get(&id)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    alert.insert("@type".to_owned(), json!("Alert"));
    alert.insert("action".to_owned(), json!("display"));
    alert.insert(
        "trigger".to_owned(),
        json!({"@type":"AbsoluteTrigger", "when":normalized}),
    );
    if alert.contains_key("acknowledged") {
        alert.remove("acknowledged");
    }
    remaining.insert(id, Value::Object(alert));
    task.insert("alerts".to_owned(), Value::Object(remaining));
    task.insert("useDefaultAlerts".to_owned(), json!(false));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_absolute_display_alerts_as_eas_utc_times() {
        let alerts = json!({"a":{"@type":"Alert","trigger":{"@type":"AbsoluteTrigger","when":"2026-11-20T11:45:00-05:00"}}});
        assert_eq!(
            read_reminder(&alerts).as_deref(),
            Some("2026-11-20T16:45:00.000Z")
        );
        assert_eq!(
            read_reminder(&json!({"a":{"trigger":{"@type":"OffsetTrigger","offset":"-PT5M"}}})),
            None
        );
        assert_eq!(read_reminder(&json!({})), None);
    }

    #[test]
    fn updates_one_display_alert_and_preserves_other_actions() {
        let alerts = json!({
            "display":{"@type":"Alert","acknowledged":"2026-01-01T00:00:00Z","trigger":{"@type":"AbsoluteTrigger","when":"2026-11-20T16:45:00Z"}},
            "email":{"@type":"Alert","action":"email","trigger":{"@type":"OffsetTrigger","offset":"-PT5M"}}
        });
        let mut patch = Map::new();
        write_reminder(
            &mut patch,
            &alerts,
            false,
            Some(true),
            Some("2026-11-21T08:00:00.000Z"),
        )
        .unwrap();
        assert_eq!(
            patch["alerts"]["display"]["trigger"]["when"],
            "2026-11-21T08:00:00Z"
        );
        assert!(patch["alerts"]["display"].get("acknowledged").is_none());
        assert!(patch["alerts"]["email"].is_object());

        let mut patch = Map::new();
        write_reminder(&mut patch, &alerts, false, Some(false), None).unwrap();
        assert!(patch["alerts"]["display"].is_null());
        assert!(patch["alerts"]["email"].is_object());
    }

    #[test]
    fn refuses_ambiguous_default_or_missing_reminder_state() {
        let mut patch = Map::new();
        assert!(write_reminder(
            &mut patch,
            &Value::Null,
            true,
            Some(true),
            Some("2026-11-20T16:45:00Z")
        )
        .is_err());
        assert!(write_reminder(&mut patch, &Value::Null, false, Some(true), None).is_err());
    }
}
