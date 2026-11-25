use crate::wbxml::eas::CalendarRecurrence;
use anyhow::Context;
use chrono::TimeZone;
use serde::Deserialize;
use serde_json::{json, Value};

const WEEKDAYS: [(&str, u8); 7] = [
    ("su", 1),
    ("mo", 2),
    ("tu", 4),
    ("we", 8),
    ("th", 16),
    ("fr", 32),
    ("sa", 64),
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NDay {
    #[serde(rename = "@type")]
    kind: Option<String>,
    day: String,
    nth_of_period: Option<i8>,
}

pub fn to_jscalendar(value: &CalendarRecurrence, time_zone: Option<&str>) -> anyhow::Result<Value> {
    to_jscalendar_with_weekday_fallback(value, time_zone, None)
}

pub fn to_jscalendar_with_weekday_fallback(
    value: &CalendarRecurrence,
    time_zone: Option<&str>,
    inferred_day_of_week: Option<u8>,
) -> anyhow::Result<Value> {
    let interval = value.interval.unwrap_or(1);
    anyhow::ensure!((1..=999).contains(&interval), "invalid recurrence interval");
    let frequency = match value.kind {
        0 if value.day_of_week.is_some() => "weekly",
        0 => "daily",
        1 => "weekly",
        2 | 3 => "monthly",
        5 | 6 => "yearly",
        _ => anyhow::bail!("unsupported recurrence type"),
    };
    let mut rule = serde_json::Map::new();
    rule.insert("frequency".into(), json!(frequency));
    if interval != 1 {
        rule.insert("interval".into(), json!(interval));
    }
    if let Some(first_day) = value.first_day_of_week {
        rule.insert("firstDayOfWeek".into(), json!(weekday_name(first_day)?));
    }
    if let Some(count) = value.occurrences {
        anyhow::ensure!(count > 0 && value.until.is_none(), "invalid recurrence end");
        rule.insert("count".into(), json!(count));
    }
    if let Some(until) = value.until.as_deref() {
        rule.insert("until".into(), json!(eas_until_to_local(until, time_zone)?));
    }

    match value.kind {
        0 if value.day_of_week.is_some() => {
            anyhow::ensure!(
                value.day_of_month.is_none()
                    && value.week_of_month.is_none()
                    && value.month_of_year.is_none(),
                "unexpected daily-weekday recurrence fields"
            );
            rule.insert(
                "byDay".into(),
                json!(weekdays(value.day_of_week.unwrap(), None)?),
            );
        }
        0 => anyhow::ensure!(
            value.day_of_month.is_none()
                && value.week_of_month.is_none()
                && value.month_of_year.is_none(),
            "unexpected daily recurrence fields"
        ),
        1 => {
            anyhow::ensure!(
                value.day_of_week.is_some(),
                "weekly recurrence needs DayOfWeek"
            );
            anyhow::ensure!(
                value.day_of_month.is_none()
                    && value.week_of_month.is_none()
                    && value.month_of_year.is_none(),
                "unexpected weekly recurrence fields"
            );
            rule.insert(
                "byDay".into(),
                json!(weekdays(value.day_of_week.unwrap(), None)?),
            );
        }
        2 => {
            let day = value
                .day_of_month
                .context("monthly recurrence needs DayOfMonth")?;
            anyhow::ensure!((1..=31).contains(&day), "invalid monthly DayOfMonth");
            anyhow::ensure!(
                value.day_of_week.is_none()
                    && value.week_of_month.is_none()
                    && value.month_of_year.is_none(),
                "unexpected monthly recurrence fields"
            );
            rule.insert("byMonthDay".into(), json!([day]));
        }
        3 => {
            let week = value
                .week_of_month
                .context("nth monthly recurrence needs WeekOfMonth")?;
            let days = value
                .day_of_week
                .context("nth monthly recurrence needs DayOfWeek")?;
            anyhow::ensure!(
                value.month_of_year.is_none() && value.day_of_month.is_none(),
                "unexpected nth monthly recurrence fields"
            );
            if days == 127 {
                anyhow::ensure!((1..=5).contains(&week), "invalid monthly day");
                rule.insert(
                    "byMonthDay".into(),
                    json!([if week == 5 { -1 } else { i64::from(week) }]),
                );
            } else {
                rule.insert("byDay".into(), json!(weekdays(days, None)?));
                rule.insert("bySetPosition".into(), json!([week_to_position(week)]));
            }
        }
        5 => {
            let day = value
                .day_of_month
                .context("yearly recurrence needs DayOfMonth")?;
            let month = value
                .month_of_year
                .context("yearly recurrence needs MonthOfYear")?;
            anyhow::ensure!(
                (1..=31).contains(&day) && (1..=12).contains(&month),
                "invalid yearly recurrence fields"
            );
            anyhow::ensure!(
                value.day_of_week.is_none() && value.week_of_month.is_none(),
                "unexpected yearly recurrence fields"
            );
            rule.insert("byMonth".into(), json!([month.to_string()]));
            rule.insert("byMonthDay".into(), json!([day]));
        }
        6 => {
            let month = value
                .month_of_year
                .context("nth yearly recurrence needs MonthOfYear")?;
            let week = value
                .week_of_month
                .context("nth yearly recurrence needs WeekOfMonth")?;
            let days = value
                .day_of_week
                .or(inferred_day_of_week)
                .context("nth yearly recurrence needs DayOfWeek or a matching series start")?;
            anyhow::ensure!(
                (1..=12).contains(&month) && value.day_of_month.is_none(),
                "invalid nth yearly recurrence fields"
            );
            rule.insert("byMonth".into(), json!([month.to_string()]));
            if days == 127 {
                anyhow::ensure!((1..=5).contains(&week), "invalid yearly day");
                rule.insert(
                    "byMonthDay".into(),
                    json!([if week == 5 { -1 } else { i64::from(week) }]),
                );
            } else {
                rule.insert("byDay".into(), json!(weekdays(days, None)?));
                rule.insert("bySetPosition".into(), json!([week_to_position(week)]));
            }
        }
        _ => unreachable!(),
    }
    Ok(Value::Object(rule))
}

/// Infer the omitted EAS yearly-nth weekday from the series' first start.
/// The start must itself match the EAS month and ordinal selectors, so this
/// cannot silently turn an inconsistent recurrence into a different rule.
pub fn infer_yearly_nth_weekday(
    recurrence: &CalendarRecurrence,
    start: chrono::NaiveDateTime,
) -> anyhow::Result<u8> {
    use chrono::Datelike;

    anyhow::ensure!(
        recurrence.kind == 6 && recurrence.day_of_week.is_none(),
        "weekday inference only applies to yearly-nth patterns with no DayOfWeek"
    );
    let month = recurrence
        .month_of_year
        .context("yearly-nth recurrence needs MonthOfYear")?;
    let week = recurrence
        .week_of_month
        .context("yearly-nth recurrence needs WeekOfMonth")?;
    anyhow::ensure!(
        start.month() == u32::from(month),
        "yearly-nth series start month does not match MonthOfYear"
    );
    let ordinal = (start.day() - 1) / 7 + 1;
    if week == 5 {
        let first = chrono::NaiveDate::from_ymd_opt(start.year(), start.month(), 1)
            .context("invalid yearly-nth start date")?;
        let next_month = if start.month() == 12 {
            chrono::NaiveDate::from_ymd_opt(start.year() + 1, 1, 1)
        } else {
            chrono::NaiveDate::from_ymd_opt(start.year(), start.month() + 1, 1)
        }
        .context("invalid yearly-nth month boundary")?;
        let days_in_month = next_month.signed_duration_since(first).num_days() as u32;
        anyhow::ensure!(
            start.day() + 7 > days_in_month,
            "yearly-nth series start is not the last matching weekday"
        );
    } else {
        anyhow::ensure!(
            ordinal == u32::from(week),
            "yearly-nth series start week does not match WeekOfMonth"
        );
    }
    Ok(1 << start.weekday().num_days_from_sunday())
}

pub fn from_jscalendar(
    rule: &Value,
    time_zone: Option<&str>,
) -> anyhow::Result<CalendarRecurrence> {
    let object = rule.as_object().context("invalid recurrence rule")?;
    let allowed = [
        "@type",
        "frequency",
        "interval",
        "rscale",
        "skip",
        "firstDayOfWeek",
        "byDay",
        "byMonthDay",
        "byMonth",
        "bySetPosition",
        "count",
        "until",
    ];
    anyhow::ensure!(
        object.keys().all(|key| allowed.contains(&key.as_str())),
        "unsupported JSCalendar recurrence property"
    );
    anyhow::ensure!(
        object
            .get("@type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "RecurrenceRule"),
        "invalid recurrence rule type"
    );
    anyhow::ensure!(
        object
            .get("rscale")
            .and_then(Value::as_str)
            .is_none_or(|v| v == "gregorian"),
        "unsupported recurrence calendar system"
    );
    anyhow::ensure!(
        object
            .get("skip")
            .and_then(Value::as_str)
            .is_none_or(|v| v == "omit"),
        "unsupported recurrence skip policy"
    );
    let frequency = rule["frequency"]
        .as_str()
        .context("recurrence frequency missing")?;
    let interval = optional_u32(rule, "interval")?;
    anyhow::ensure!(
        interval.is_none_or(|value| (1..=999).contains(&value)),
        "invalid recurrence interval"
    );
    let occurrences = optional_u32(rule, "count")?;
    anyhow::ensure!(
        occurrences.is_none_or(|value| value > 0),
        "invalid recurrence count"
    );
    let until = rule["until"]
        .as_str()
        .map(|value| local_until_to_eas(value, time_zone))
        .transpose()?;
    anyhow::ensure!(
        occurrences.is_none() || until.is_none(),
        "recurrence has both count and until"
    );
    let first_day_of_week = rule["firstDayOfWeek"]
        .as_str()
        .map(weekday_value)
        .transpose()?;
    let by_day = nday_array(rule, "byDay")?;
    let by_month_day = number_array(rule, "byMonthDay")?;
    let by_month = string_array(rule, "byMonth")?;
    let by_set_position = number_array(rule, "bySetPosition")?;
    anyhow::ensure!(
        by_set_position.len() <= 1,
        "unsupported recurrence set position"
    );
    let set_position = by_set_position
        .first()
        .map(|value| i8::try_from(*value))
        .transpose()
        .context("invalid recurrence set position")?;

    let (kind, day_of_week, day_of_month, week_of_month, month_of_year) = match frequency {
        "daily" => {
            anyhow::ensure!(
                by_day.is_empty()
                    && by_month_day.is_empty()
                    && by_month.is_empty()
                    && set_position.is_none(),
                "unsupported daily recurrence rule"
            );
            (0, None, None, None, None)
        }
        "weekly" => {
            anyhow::ensure!(
                by_month_day.is_empty() && by_month.is_empty() && set_position.is_none(),
                "unsupported weekly recurrence rule"
            );
            (1, Some(day_mask(&by_day, false)?), None, None, None)
        }
        "monthly"
            if by_month_day == [-1]
                && by_day.is_empty()
                && by_month.is_empty()
                && set_position.is_none() =>
        {
            (3, Some(127), None, Some(5), None)
        }
        "monthly"
            if by_month_day.len() == 1
                && by_day.is_empty()
                && by_month.is_empty()
                && set_position.is_none() =>
        {
            let day = u8::try_from(by_month_day[0]).context("invalid monthly day")?;
            anyhow::ensure!((1..=31).contains(&day), "invalid monthly day");
            (2, None, Some(day), None, None)
        }
        "monthly" if !by_day.is_empty() && by_month_day.is_empty() && by_month.is_empty() => {
            let week = set_position.context("monthly weekday recurrence needs set position")?;
            (
                3,
                Some(day_mask(&by_day, false)?),
                None,
                Some(ordinal_to_week(week)?),
                None,
            )
        }
        "yearly"
            if by_month.len() == 1
                && by_month_day == [-1]
                && by_day.is_empty()
                && set_position.is_none() =>
        {
            let month = parse_month(&by_month[0])?;
            (6, Some(127), None, Some(5), Some(month))
        }
        "yearly"
            if by_month.len() == 1
                && by_month_day.len() == 1
                && by_day.is_empty()
                && set_position.is_none() =>
        {
            let month = parse_month(&by_month[0])?;
            let day = u8::try_from(by_month_day[0]).context("invalid yearly day")?;
            anyhow::ensure!((1..=31).contains(&day), "invalid yearly recurrence date");
            (5, None, Some(day), None, Some(month))
        }
        "yearly" if by_month.len() == 1 && !by_day.is_empty() && by_month_day.is_empty() => {
            let month = parse_month(&by_month[0])?;
            let week = set_position.context("yearly weekday recurrence needs set position")?;
            (
                6,
                Some(day_mask(&by_day, false)?),
                None,
                Some(ordinal_to_week(week)?),
                Some(month),
            )
        }
        _ => anyhow::bail!("unsupported JSCalendar recurrence rule"),
    };
    Ok(CalendarRecurrence {
        kind,
        interval,
        day_of_week,
        day_of_month,
        week_of_month,
        month_of_year,
        first_day_of_week,
        occurrences,
        until,
    })
}

fn weekdays(mask: u8, ordinal: Option<i8>) -> anyhow::Result<Vec<Value>> {
    anyhow::ensure!(mask != 0 && mask <= 127, "invalid recurrence weekday mask");
    Ok(WEEKDAYS
        .iter()
        .filter(|(_, bit)| mask & bit != 0)
        .map(|(day, _)| {
            let mut value = json!({"day":day});
            if let Some(n) = ordinal {
                value["nthOfPeriod"] = json!(n);
            }
            value
        })
        .collect())
}

fn day_mask(days: &[NDay], require_ordinal: bool) -> anyhow::Result<u8> {
    anyhow::ensure!(!days.is_empty(), "recurrence weekdays missing");
    let mut mask = 0u8;
    for day in days {
        anyhow::ensure!(
            day.kind.as_deref().is_none_or(|kind| kind == "NDay"),
            "invalid recurrence day type"
        );
        anyhow::ensure!(
            require_ordinal == day.nth_of_period.is_some(),
            "invalid recurrence weekday ordinal"
        );
        if let Some(ordinal) = day.nth_of_period {
            anyhow::ensure!(ordinal != 0, "invalid recurrence weekday ordinal");
        }
        let (_, bit) = WEEKDAYS
            .iter()
            .find(|(name, _)| *name == day.day)
            .context("invalid recurrence weekday")?;
        anyhow::ensure!(mask & bit == 0, "duplicate recurrence weekday");
        mask |= bit;
    }
    Ok(mask)
}

fn nday_array(rule: &Value, key: &str) -> anyhow::Result<Vec<NDay>> {
    rule.get(key)
        .map(|value| serde_json::from_value(value.clone()).context("invalid recurrence day list"))
        .transpose()
        .map(Option::unwrap_or_default)
}
fn string_array(rule: &Value, key: &str) -> anyhow::Result<Vec<String>> {
    rule.get(key)
        .map(|value| {
            serde_json::from_value(value.clone()).context("invalid recurrence string list")
        })
        .transpose()
        .map(Option::unwrap_or_default)
}
fn number_array(rule: &Value, key: &str) -> anyhow::Result<Vec<i64>> {
    rule.get(key)
        .map(|value| {
            serde_json::from_value(value.clone()).context("invalid recurrence number list")
        })
        .transpose()
        .map(Option::unwrap_or_default)
}
fn optional_u32(rule: &Value, key: &str) -> anyhow::Result<Option<u32>> {
    rule.get(key)
        .map(|value| {
            let value = value.as_u64().context("invalid recurrence number")?;
            u32::try_from(value).context("invalid recurrence number")
        })
        .transpose()
}
fn parse_month(value: &str) -> anyhow::Result<u8> {
    let month = value.parse::<u8>().context("invalid recurrence month")?;
    anyhow::ensure!((1..=12).contains(&month), "invalid recurrence month");
    Ok(month)
}
fn week_to_position(week: u8) -> i8 {
    if week == 5 {
        -1
    } else {
        week as i8
    }
}
fn ordinal_to_week(ordinal: i8) -> anyhow::Result<u8> {
    match ordinal {
        -1 => Ok(5),
        1..=4 => Ok(ordinal as u8),
        _ => anyhow::bail!("unsupported recurrence ordinal"),
    }
}
fn weekday_name(value: u8) -> anyhow::Result<&'static str> {
    WEEKDAYS
        .get(value as usize)
        .map(|(name, _)| *name)
        .context("invalid first day of week")
}
fn weekday_value(value: &str) -> anyhow::Result<u8> {
    WEEKDAYS
        .iter()
        .position(|(name, _)| *name == value)
        .and_then(|day| u8::try_from(day).ok())
        .context("invalid first day of week")
}

fn eas_until_to_local(value: &str, time_zone: Option<&str>) -> anyhow::Result<String> {
    let utc = chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map(|date| date.and_utc())
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(value).map(|date| date.with_timezone(&chrono::Utc))
        })
        .context("invalid EAS recurrence Until")?;
    let local = match time_zone {
        Some(name) => {
            let zone: chrono_tz::Tz = name.parse().context("invalid calendar time zone")?;
            utc.with_timezone(&zone).naive_local()
        }
        None => utc.naive_utc(),
    };
    Ok(local.format("%Y-%m-%dT%H:%M:%S").to_string())
}
fn local_until_to_eas(value: &str, time_zone: Option<&str>) -> anyhow::Result<String> {
    let local = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .context("invalid recurrence until")?;
    let utc = match time_zone {
        Some(name) => {
            let zone: chrono_tz::Tz = name.parse().context("invalid calendar time zone")?;
            match zone.from_local_datetime(&local) {
                chrono::LocalResult::Single(value) => value.with_timezone(&chrono::Utc).naive_utc(),
                chrono::LocalResult::Ambiguous(_, _) | chrono::LocalResult::None => {
                    anyhow::bail!("recurrence until is ambiguous in its time zone")
                }
            }
        }
        None => local,
    };
    Ok(utc.format("%Y%m%dT%H%M%SZ").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eas_patterns_map_to_jscalendar_and_back() {
        for recurrence in [
            CalendarRecurrence {
                kind: 0,
                interval: Some(2),
                occurrences: Some(10),
                ..Default::default()
            },
            CalendarRecurrence {
                kind: 1,
                day_of_week: Some(10),
                first_day_of_week: Some(1),
                ..Default::default()
            },
            CalendarRecurrence {
                kind: 2,
                day_of_month: Some(31),
                ..Default::default()
            },
            CalendarRecurrence {
                kind: 3,
                day_of_week: Some(62),
                week_of_month: Some(2),
                ..Default::default()
            },
            CalendarRecurrence {
                kind: 5,
                day_of_month: Some(14),
                month_of_year: Some(2),
                until: Some("20270301T000000Z".into()),
                ..Default::default()
            },
            CalendarRecurrence {
                kind: 6,
                day_of_week: Some(2),
                week_of_month: Some(5),
                month_of_year: Some(11),
                ..Default::default()
            },
        ] {
            let jscalendar = to_jscalendar(&recurrence, None).unwrap();
            assert!(jscalendar["@type"].is_null());
            assert_eq!(from_jscalendar(&jscalendar, None).unwrap(), recurrence);
        }
    }

    #[test]
    fn yearly_nth_without_day_of_week_uses_only_a_matching_series_start() {
        let recurrence = CalendarRecurrence {
            kind: 6,
            week_of_month: Some(5),
            month_of_year: Some(11),
            ..Default::default()
        };
        let start = chrono::NaiveDate::from_ymd_opt(2026, 11, 30)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let weekday = infer_yearly_nth_weekday(&recurrence, start).unwrap();
        assert_eq!(weekday, 2); // Monday
        let rule = to_jscalendar_with_weekday_fallback(&recurrence, None, Some(weekday)).unwrap();
        assert_eq!(rule["frequency"], "yearly");
        assert_eq!(rule["byMonth"], json!(["11"]));
        assert_eq!(rule["byDay"], json!([{"day":"mo"}]));
        assert_eq!(rule["bySetPosition"], json!([-1]));
        assert!(to_jscalendar(&recurrence, None).is_err());

        let wrong_month = chrono::NaiveDate::from_ymd_opt(2026, 10, 26)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        assert!(infer_yearly_nth_weekday(&recurrence, wrong_month).is_err());
        let not_last = chrono::NaiveDate::from_ymd_opt(2026, 11, 23)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        assert!(infer_yearly_nth_weekday(&recurrence, not_last).is_err());
    }

    #[test]
    fn recurrence_until_translates_between_utc_and_event_local_time() {
        let recurrence = CalendarRecurrence {
            kind: 0,
            until: Some("20261125T170000Z".into()),
            ..Default::default()
        };
        let rule = to_jscalendar(&recurrence, Some("America/Toronto")).unwrap();
        assert_eq!(rule["until"], "2026-11-25T12:00:00");
        assert_eq!(
            from_jscalendar(&rule, Some("America/Toronto")).unwrap(),
            recurrence
        );
    }

    #[test]
    fn rejects_unrepresentable_jscalendar_rules() {
        for rule in [
            json!({"@type":"RecurrenceRule","frequency":"weekly","byDay":[{"@type":"NDay","day":"mo","nthOfPeriod":1}]}),
            json!({"@type":"RecurrenceRule","frequency":"monthly","byMonthDay":[1,2]}),
            json!({"@type":"RecurrenceRule","frequency":"yearly","byMonth":["1"],"byDay":[{"@type":"NDay","day":"mo"}]}),
        ] {
            assert!(from_jscalendar(&rule, None).is_err(), "{rule}");
        }
    }

    #[test]
    fn recurrence_interval_and_count_must_be_unsigned_32_bit_integers() {
        for (field, value) in [
            ("interval", json!("2")),
            ("interval", json!(-1)),
            ("interval", json!(1000)),
            ("interval", json!(4294967296u64)),
            ("count", json!("5")),
            ("count", json!(-1)),
            ("count", json!(4294967296u64)),
        ] {
            let rule = json!({"frequency":"daily", field:value});
            assert!(from_jscalendar(&rule, None).is_err(), "{rule}");
        }

        let max_interval = json!({"frequency":"daily","interval":999});
        assert_eq!(
            from_jscalendar(&max_interval, None).unwrap().interval,
            Some(999)
        );
        let rule = json!({"frequency":"daily","interval":1,"count":4294967295u64});
        assert_eq!(
            from_jscalendar(&rule, None).unwrap().occurrences,
            Some(u32::MAX)
        );
    }

    #[test]
    fn fifth_day_of_week_127_recurrence_maps_to_last_day_of_month() {
        for recurrence in [
            CalendarRecurrence {
                kind: 3,
                week_of_month: Some(5),
                day_of_week: Some(127),
                ..Default::default()
            },
            CalendarRecurrence {
                kind: 6,
                week_of_month: Some(5),
                day_of_week: Some(127),
                month_of_year: Some(6),
                ..Default::default()
            },
        ] {
            let rule = to_jscalendar(&recurrence, None).unwrap();
            assert_eq!(rule["byMonthDay"], json!([-1]));
            assert_eq!(from_jscalendar(&rule, None).unwrap(), recurrence);
        }

        for (week, day) in [(1, 1), (4, 4)] {
            let recurrence = CalendarRecurrence {
                kind: 3,
                week_of_month: Some(week),
                day_of_week: Some(127),
                ..Default::default()
            };
            assert_eq!(
                to_jscalendar(&recurrence, None).unwrap()["byMonthDay"],
                json!([day])
            );
        }

        for rule in [
            json!({"frequency":"monthly","byMonthDay":[-2]}),
            json!({"frequency":"yearly","byMonth":["6"],"byMonthDay":[-2]}),
        ] {
            assert!(from_jscalendar(&rule, None).is_err(), "{rule}");
        }
    }
}
