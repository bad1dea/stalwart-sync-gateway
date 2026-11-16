use anyhow::bail;
use serde_json::{json, Map, Value};

fn minutes(offset: &str) -> Option<u32> {
    let (negative, raw) = match offset.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, offset),
    };
    let bytes = raw.strip_prefix('P')?.as_bytes();
    let mut index = 0;
    let mut in_time = false;
    let mut last = 0;
    let mut total = 0u64;
    let mut count = 0;
    let mut weeks = false;
    while index < bytes.len() {
        if bytes[index] == b'T' {
            if in_time || weeks {
                return None;
            }
            in_time = true;
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start || index == bytes.len() {
            return None;
        }
        let number = std::str::from_utf8(&bytes[start..index])
            .ok()?
            .parse::<u64>()
            .ok()?;
        let (order, multiplier) = match (in_time, bytes[index]) {
            (false, b'W') => {
                weeks = true;
                (1, 604800u64)
            }
            (false, b'D') => (2, 86400),
            (true, b'H') => (3, 3600),
            (true, b'M') => (4, 60),
            (true, b'S') => (5, 1),
            _ => return None,
        };
        if order <= last || (weeks && (count > 0 || index + 1 != bytes.len())) {
            return None;
        }
        last = order;
        total = total.checked_add(number.checked_mul(multiplier)?)?;
        count += 1;
        index += 1;
    }
    if count == 0 || (in_time && last < 3) || (!negative && total != 0) || !total.is_multiple_of(60)
    {
        return None;
    }
    u32::try_from(total / 60).ok()
}

fn display_alerts(alerts: &Value) -> anyhow::Result<Vec<(String, Option<u32>)>> {
    let Some(map) = alerts.as_object() else {
        if alerts.is_null() {
            return Ok(Vec::new());
        }
        bail!("invalid calendar alerts");
    };
    let mut result = Vec::new();
    for (id, alert) in map {
        let action = alert
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("display");
        if action != "display" {
            continue;
        }
        let trigger = &alert["trigger"];
        let relative = trigger
            .get("relativeTo")
            .and_then(Value::as_str)
            .unwrap_or("start");
        let value = if trigger["@type"] == "OffsetTrigger" && relative == "start" {
            trigger["offset"].as_str().and_then(minutes)
        } else {
            None
        };
        result.push((id.clone(), value));
    }
    Ok(result)
}

pub fn read_reminder(alerts: &Value) -> Option<u32> {
    display_alerts(alerts)
        .ok()?
        .into_iter()
        .filter_map(|(_, value)| value)
        .min()
}

pub fn unchanged(alerts: &Value, reminder: Option<u32>) -> anyhow::Result<bool> {
    let display = display_alerts(alerts)?;
    Ok(match reminder {
        Some(minutes) => display.iter().filter_map(|(_, value)| *value).min() == Some(minutes),
        None => display.is_empty(),
    })
}

pub fn defaults_match(alerts: &Value, defaults: &Value) -> bool {
    fn normalized(value: &Value) -> Option<String> {
        let mut alert = value.as_object()?.clone();
        if alert.get("@type").is_some_and(|kind| kind != "Alert") {
            return None;
        }
        alert.insert("@type".to_owned(), json!("Alert"));
        if !alert.contains_key("action") {
            alert.insert("action".to_owned(), json!("display"));
        }
        if let Some(trigger) = alert.get_mut("trigger").and_then(Value::as_object_mut) {
            if trigger
                .get("@type")
                .is_some_and(|kind| kind == "OffsetTrigger")
                && !trigger.contains_key("relativeTo")
            {
                trigger.insert("relativeTo".to_owned(), json!("start"));
            }
        }
        serde_json::to_string(&alert).ok()
    }
    let Some(alerts) = alerts.as_object() else {
        return false;
    };
    let Some(defaults) = defaults.as_object() else {
        return false;
    };
    let Some(mut actual) = alerts.values().map(normalized).collect::<Option<Vec<_>>>() else {
        return false;
    };
    let Some(mut expected) = defaults
        .values()
        .map(normalized)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    actual.sort();
    expected.sort();
    actual == expected
}

pub fn write_reminder(
    event: &mut Map<String, Value>,
    alerts: &Value,
    reminder: Option<u32>,
) -> anyhow::Result<()> {
    let existing = display_alerts(alerts)?;
    if existing.len() > 1 || existing.iter().any(|(_, value)| value.is_none()) {
        bail!("calendar has display alerts that cannot be edited safely");
    }
    event.insert("useDefaultAlerts".into(), json!(false));
    let alert = |minutes: u32| {
        json!({
            "@type":"Alert", "action":"display",
            "trigger":{"@type":"OffsetTrigger","relativeTo":"start",
                "offset": if minutes == 0 { "PT0S".to_owned() } else { format!("-PT{minutes}M") }}
        })
    };
    match (existing.first(), reminder) {
        (Some((id, _)), Some(value)) => {
            let escaped = id.replace('~', "~0").replace('/', "~1");
            event.insert(
                format!("alerts/{escaped}/trigger"),
                alert(value)["trigger"].clone(),
            );
            if alerts[id].get("acknowledged").is_some() {
                event.insert(format!("alerts/{escaped}/acknowledged"), Value::Null);
            }
        }
        (Some((id, _)), None) => {
            let escaped = id.replace('~', "~0").replace('/', "~1");
            event.insert(format!("alerts/{escaped}"), Value::Null);
        }
        (None, Some(value)) => {
            let id = format!("eas{}", uuid::Uuid::new_v4().simple());
            if alerts.as_object().is_some_and(|map| !map.is_empty()) {
                event.insert(format!("alerts/{id}"), alert(value));
            } else {
                event.insert("alerts".into(), json!({id:alert(value)}));
            }
        }
        (None, None) => {
            if !alerts.is_null() && !alerts.is_object() {
                bail!("invalid calendar alerts");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_representable_offsets() {
        for (text, expected) in [
            ("-PT15M", Some(15)),
            ("-P1DT2H30M", Some(1590)),
            ("-P1W", Some(10080)),
            ("PT0S", Some(0)),
            ("-PT60S", Some(1)),
            ("PT15M", None),
            ("-PT10S", None),
            ("-P", None),
            ("-PT", None),
            ("-P1WT2H", None),
            ("-PT999999999999999999999H", None),
        ] {
            assert_eq!(minutes(text), expected, "{text}");
        }
    }

    #[test]
    fn unchanged_reminders_preserve_other_alerts() {
        let alerts = json!({"a":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","offset":"-PT20M"}},
            "other":{"@type":"Alert","trigger":{"@type":"AbsoluteTrigger","when":"2042-01-01T00:00:00Z"}}});
        assert!(unchanged(&alerts, Some(20)).unwrap());
        assert!(!unchanged(&alerts, None).unwrap());
        assert!(unchanged(&Value::Null, None).unwrap());
    }

    #[test]
    fn matches_materialized_defaults_without_activating_extra_alerts() {
        let defaults = json!({"a":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","offset":"-PT20M","relativeTo":"start"}}});
        let mut actual = json!({"generated":{"@type":"Alert","action":"display","trigger":{"@type":"OffsetTrigger","offset":"-PT20M"}}});
        assert!(defaults_match(&actual, &defaults));
        actual["other"] = json!({"@type":"Alert","action":"email","trigger":{"@type":"OffsetTrigger","offset":"-PT1H"}});
        assert!(!defaults_match(&actual, &defaults));
    }

    #[test]
    fn edits_one_display_alert_and_preserves_other_actions() {
        let alerts = json!({"id/one":{"@type":"Alert","action":"display","acknowledged":"2026-01-01T00:00:00Z",
            "trigger":{"@type":"OffsetTrigger","offset":"-PT15M"}},
            "email":{"@type":"Alert","action":"email","trigger":{"@type":"OffsetTrigger","offset":"-PT1H"}}});
        assert_eq!(read_reminder(&alerts), Some(15));
        let mut patch = Map::new();
        write_reminder(&mut patch, &alerts, Some(5)).unwrap();
        assert_eq!(patch["alerts/id~1one/trigger"]["offset"], "-PT5M");
        assert_eq!(patch["alerts/id~1one/acknowledged"], Value::Null);
        assert!(!patch.contains_key("alerts/email"));
        let mut patch = Map::new();
        write_reminder(&mut patch, &alerts, None).unwrap();
        assert_eq!(patch["alerts/id~1one"], Value::Null);
        let mut many = alerts;
        many["another"] =
            json!({"@type":"Alert","trigger":{"@type":"OffsetTrigger","offset":"-PT10M"}});
        assert!(write_reminder(&mut Map::new(), &many, None).is_err());
    }
}
