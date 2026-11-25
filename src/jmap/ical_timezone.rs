use chrono::Offset as _;
use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::collections::BTreeMap;

type Transition = (NaiveDateTime, i32, i32);

fn offset_at(zone: Tz, timestamp: i64) -> Option<i32> {
    let utc = chrono::DateTime::<Utc>::from_timestamp(timestamp, 0)?;
    Some(
        zone.offset_from_utc_datetime(&utc.naive_utc())
            .fix()
            .local_minus_utc(),
    )
}

fn transitions(zone: Tz, year: i32) -> Option<Vec<Transition>> {
    let start = NaiveDate::from_ymd_opt(year, 1, 1)?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp();
    let end = NaiveDate::from_ymd_opt(year + 1, 1, 1)?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp();
    let mut previous = start;
    let mut previous_offset = offset_at(zone, previous)?;
    let mut output = Vec::new();
    let mut sample = previous + 6 * 60 * 60;
    while sample <= end {
        let current_offset = offset_at(zone, sample)?;
        if current_offset != previous_offset {
            let (mut low, mut high) = (previous, sample);
            while high - low > 1 {
                let middle = low + (high - low) / 2;
                if offset_at(zone, middle)? == previous_offset {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            let local_before =
                chrono::DateTime::<Utc>::from_timestamp(high + i64::from(previous_offset), 0)?
                    .naive_utc();
            output.push((local_before, previous_offset, current_offset));
            previous_offset = current_offset;
        }
        previous = sample;
        sample += 6 * 60 * 60;
    }
    Some(output)
}

fn weekday_name(day: chrono::Weekday) -> &'static str {
    match day {
        chrono::Weekday::Sun => "SU",
        chrono::Weekday::Mon => "MO",
        chrono::Weekday::Tue => "TU",
        chrono::Weekday::Wed => "WE",
        chrono::Weekday::Thu => "TH",
        chrono::Weekday::Fri => "FR",
        chrono::Weekday::Sat => "SA",
    }
}

fn offset_text(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let seconds = offset.unsigned_abs();
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let remainder = seconds % 60;
    if remainder == 0 {
        format!("{sign}{hours:02}{minutes:02}")
    } else {
        format!("{sign}{hours:02}{minutes:02}{remainder:02}")
    }
}

fn transition_rule(value: NaiveDateTime) -> Option<(u32, String)> {
    let day = value.day();
    let days_in_month = NaiveDate::from_ymd_opt(value.year(), value.month(), 1)?
        .with_month(value.month() + 1)
        .or_else(|| NaiveDate::from_ymd_opt(value.year() + 1, 1, 1))?
        .pred_opt()?
        .day();
    let ordinal = if day + 7 > days_in_month {
        -1
    } else {
        ((day - 1) / 7 + 1) as i8
    };
    Some((
        value.month(),
        format!("{ordinal}{}", weekday_name(value.weekday())),
    ))
}

/// Create a recurring VTIMEZONE for zones whose transition pattern repeats
/// over the next four years, or a fixed-offset definition for non-DST zones.
/// Zones with irregular near-term transitions are rejected rather than being
/// represented by a misleading timezone rule.
pub fn component(tzid: &str, year: i32) -> Option<String> {
    let zone = tzid.parse::<Tz>().ok()?;
    let mut transitions_by_year = Vec::new();
    for sample_year in year..year.checked_add(4)? {
        transitions_by_year.push(transitions(zone, sample_year)?);
    }
    let mut output = format!("BEGIN:VTIMEZONE\r\nTZID:{tzid}\r\n");
    if transitions_by_year.iter().all(Vec::is_empty) {
        let offset = offset_at(
            zone,
            NaiveDate::from_ymd_opt(year, 1, 1)?
                .and_hms_opt(0, 0, 0)?
                .and_utc()
                .timestamp(),
        )?;
        if transitions_by_year.iter().enumerate().any(|(index, _)| {
            let sample_year = year + index as i32;
            let january = NaiveDate::from_ymd_opt(sample_year, 1, 1)
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc().timestamp());
            let july = NaiveDate::from_ymd_opt(sample_year, 7, 1)
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc().timestamp());
            january.and_then(|timestamp| offset_at(zone, timestamp)) != Some(offset)
                || july.and_then(|timestamp| offset_at(zone, timestamp)) != Some(offset)
        }) {
            return None;
        }
        output.push_str(&format!(
            "BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:{0}\r\nTZOFFSETTO:{0}\r\nEND:STANDARD\r\n",
            offset_text(offset)
        ));
    } else {
        let mut canonical = BTreeMap::<(i32, i32), (NaiveDateTime, u32, String)>::new();
        for (index, yearly) in transitions_by_year.iter().enumerate() {
            if yearly.len() != 2 {
                return None;
            }
            let mut observed = BTreeMap::new();
            for (local, before, after) in yearly {
                let pattern = transition_rule(*local)?;
                if observed.insert((*before, *after), pattern).is_some() {
                    return None;
                }
            }
            if observed.len() != 2 {
                return None;
            }
            if index == 0 {
                for (offsets, (month, weekday)) in observed {
                    let local = yearly
                        .iter()
                        .find(|(_, before, after)| (*before, *after) == offsets)?
                        .0;
                    canonical.insert(offsets, (local, month, weekday));
                }
            } else {
                if observed.len() != canonical.len() {
                    return None;
                }
                for (offsets, (month, weekday)) in observed {
                    let (_, expected_month, expected_weekday) = canonical.get(&offsets)?;
                    if month != *expected_month || weekday != *expected_weekday {
                        return None;
                    }
                }
            }
        }
        for ((before, after), (local, month, weekday)) in canonical {
            output.push_str(&format!(
                "BEGIN:{}\r\nDTSTART:{}\r\nTZOFFSETFROM:{}\r\nTZOFFSETTO:{}\r\nRRULE:FREQ=YEARLY;BYMONTH={month};BYDAY={weekday}\r\nEND:{}\r\n",
                if after > before { "DAYLIGHT" } else { "STANDARD" },
                local.format("%Y%m%dT%H%M%S"),
                offset_text(before),
                offset_text(after),
                if after > before { "DAYLIGHT" } else { "STANDARD" },
            ));
        }
    }
    output.push_str("END:VTIMEZONE\r\n");
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::component;

    #[test]
    fn daylight_zone_emits_stable_recurring_transitions() {
        let value = component("America/Toronto", 2026).unwrap();
        assert!(value.starts_with("BEGIN:VTIMEZONE\r\nTZID:America/Toronto\r\n"));
        assert!(value.contains(
            "TZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU"
        ));
        assert!(value.contains(
            "TZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nRRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU"
        ));
    }

    #[test]
    fn fixed_offset_zone_emits_one_standard_observance() {
        let value = component("Asia/Kolkata", 2026).unwrap();
        assert!(value.contains("BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0530\r\nTZOFFSETTO:+0530\r\nEND:STANDARD"));
        assert!(!value.contains("RRULE:"));
    }

    #[test]
    fn unknown_zone_fails_closed() {
        assert!(component("Not/AZone", 2026).is_none());
    }
}
