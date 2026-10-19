//! Full iCalendar trees for editing; server-expanded instances for bounded reads.
//! Never round-trip an update through the public, intentionally small event DTO.
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use icalendar::parser::{self, Calendar, Component, Property};

use super::{CalendarCollection, CalendarEvent, CalendarEventsRequest, CalendarUpdateRequest};
use crate::error::{Result, YacliError};

fn invalid(message: &str) -> YacliError {
    YacliError::Validation(message.to_string())
}

pub(super) fn document(input: &str) -> Result<Calendar<'_>> {
    let mut roots =
        parser::read_calendar_simple(input).map_err(|_| invalid("invalid iCalendar document"))?;
    if roots.len() != 1 || roots[0].name != "VCALENDAR" {
        return Err(invalid("expected exactly one VCALENDAR"));
    }
    let root = roots.remove(0);
    let mut doc = Calendar {
        properties: root.properties,
        components: root.components,
    };
    // The library strips quotes from single parameter values. Preserve their
    // original lexical form, including quoted commas versus parameter lists.
    let mut lines = input.lines().filter(|line| {
        !line.trim().is_empty() && !line.starts_with("BEGIN:") && !line.starts_with("END:")
    });
    restore_parameter_lexemes(&mut doc.properties, &mut lines)?;
    for component in &mut doc.components {
        restore_component_lexemes(component, &mut lines)?;
        if component.name == "VEVENT" {
            for name in [
                "UID",
                "DTSTART",
                "DTEND",
                "DURATION",
                "RECURRENCE-ID",
                "SUMMARY",
                "DESCRIPTION",
                "LOCATION",
                "STATUS",
                "ORGANIZER",
                "SEQUENCE",
            ] {
                let count = component
                    .properties
                    .iter()
                    .filter(|p| p.name == name)
                    .count();
                if count > 1 || (name == "UID" && count != 1) {
                    return Err(invalid(
                        "missing UID or duplicate singleton VEVENT property",
                    ));
                }
            }
        }
    }
    if lines.next().is_some() {
        return Err(invalid("unparsed iCalendar properties"));
    }
    Ok(doc)
}

fn restore_component_lexemes<'a>(
    component: &mut Component<'_>,
    lines: &mut impl Iterator<Item = &'a str>,
) -> Result<()> {
    restore_parameter_lexemes(&mut component.properties, lines)?;
    for child in &mut component.components {
        restore_component_lexemes(child, lines)?;
    }
    Ok(())
}

fn restore_parameter_lexemes<'a>(
    properties: &mut [Property<'_>],
    lines: &mut impl Iterator<Item = &'a str>,
) -> Result<()> {
    for property in properties {
        let raw = lines
            .next()
            .ok_or_else(|| invalid("missing iCalendar property"))?;
        let parsed = Property::try_from(raw).map_err(|_| invalid("invalid iCalendar property"))?;
        if *property != parsed {
            return Err(invalid("unsupported interleaved iCalendar property order"));
        }
        // Only delimiters outside quoted parameter values separate fields.
        let mut quoted = false;
        let mut start = 0;
        let mut fields = Vec::new();
        for (index, ch) in raw.char_indices() {
            if ch == '"' {
                quoted = !quoted;
            }
            if !quoted && (ch == ';' || ch == ':') {
                fields.push(&raw[start..index]);
                start = index + 1;
                if ch == ':' {
                    break;
                }
            }
        }
        if fields.len() != property.params.len() + 1 {
            return Err(invalid("unsupported iCalendar parameters"));
        }
        for (parameter, raw) in property.params.iter_mut().zip(fields.into_iter().skip(1)) {
            if let Some((_, raw_value)) = raw.split_once('=')
                && parameter.val.is_some()
            {
                parameter.val = Some(raw_value.to_string().into());
            }
        }
    }
    Ok(())
}

fn value<'a>(component: &'a Component<'_>, name: &str) -> Option<&'a str> {
    component.find_prop(name).map(|p| p.val.as_str())
}

fn uid(component: &Component<'_>) -> Option<String> {
    value(component, "UID").map(super::unescape_ical_text)
}

fn parameter<'a>(property: &'a Property<'_>, key: &str) -> Option<&'a str> {
    property
        .params
        .iter()
        .find(|p| p.key == key)
        .and_then(|p| p.val.as_ref())
        .map(|v| v.as_str().trim_matches('"'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EventTime {
    Date(NaiveDate),
    Instant(DateTime<Utc>),
}

impl EventTime {
    pub(super) fn input(input: &str) -> Result<Self> {
        if let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
            return Ok(Self::Date(date));
        }
        DateTime::parse_from_rfc3339(input)
            .map_err(|_| invalid("expected RFC3339 with an offset"))
            .and_then(|dt| {
                if dt.timestamp_subsec_nanos() != 0 {
                    return Err(invalid("iCalendar times require whole seconds"));
                }
                Ok(Self::Instant(dt.with_timezone(&Utc)))
            })
            .map_err(|_| {
                invalid(
                    "use YYYY-MM-DD for all-day events or RFC3339 with an offset for timed events",
                )
            })
    }

    pub(super) fn instant(&self) -> DateTime<Utc> {
        match self {
            Self::Date(date) => date.and_hms_opt(0, 0, 0).expect("midnight").and_utc(),
            Self::Instant(dt) => *dt,
        }
    }

    pub(super) fn display(&self) -> String {
        match self {
            Self::Date(date) => date.to_string(),
            Self::Instant(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }

    fn shifted(&self, duration: Duration) -> Result<Self> {
        let next = self
            .instant()
            .checked_add_signed(duration)
            .ok_or_else(|| invalid("calendar date overflow"))?;
        match self {
            Self::Date(_) if duration.num_seconds() % 86400 != 0 => {
                Err(invalid("all-day duration must contain whole days"))
            }
            Self::Date(_) => Ok(Self::Date(next.date_naive())),
            Self::Instant(_) => Ok(Self::Instant(next)),
        }
    }
}

fn property_time(property: &Property<'_>) -> Result<EventTime> {
    for key in ["TZID", "VALUE"] {
        if property.params.iter().filter(|p| p.key == key).count() > 1 {
            return Err(invalid("duplicate iCalendar time parameter"));
        }
    }
    let raw = property.val.as_str();
    if parameter(property, "TZID").is_some()
        && (raw.ends_with('Z') || parameter(property, "VALUE") == Some("DATE"))
    {
        return Err(invalid("TZID cannot accompany UTC or DATE values"));
    }
    if parameter(property, "VALUE") == Some("DATE") {
        return NaiveDate::parse_from_str(raw, "%Y%m%d")
            .map(EventTime::Date)
            .map_err(|_| invalid("invalid iCalendar DATE"));
    }
    if parameter(property, "VALUE").is_some_and(|v| v != "DATE-TIME") {
        return Err(invalid("unsupported iCalendar time value type"));
    }
    if raw.ends_with('Z') {
        return NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%SZ")
            .map(|v| EventTime::Instant(v.and_utc()))
            .map_err(|_| invalid("invalid UTC iCalendar time"));
    }
    let local = NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%S")
        .map_err(|_| invalid("invalid iCalendar DATE-TIME"))?;
    let tzid = parameter(property, "TZID").ok_or_else(|| {
        invalid(
            "floating times are unsupported; configure the calendar timezone or use UTC expansion",
        )
    })?;
    let tz: chrono_tz::Tz = tzid.parse().map_err(|_| {
        invalid("custom VTIMEZONE is unsupported for local conversion; use server UTC expansion")
    })?;
    let instant = tz.from_local_datetime(&local).single().ok_or_else(|| {
        invalid("ambiguous or nonexistent local time at a daylight-saving transition")
    })?;
    Ok(EventTime::Instant(instant.with_timezone(&Utc)))
}

fn required_time(component: &Component<'_>, name: &str) -> Result<EventTime> {
    property_time(
        component
            .find_prop(name)
            .ok_or_else(|| invalid(&format!("VEVENT missing {name}")))?,
    )
}

// RFC 5545 durations contain weeks OR days/time, never months or years.
struct CalendarDuration {
    days: u64,
    seconds: i64,
}

fn duration(input: &str) -> Result<CalendarDuration> {
    let input = input.strip_prefix('+').unwrap_or(input);
    let mut rest = input
        .strip_prefix('P')
        .ok_or_else(|| invalid("invalid iCalendar DURATION"))?;
    let mut days = 0u64;
    let mut seconds = 0i64;
    let mut in_time = false;
    let mut last_rank = 0;
    let mut count = 0;
    let mut weeks = false;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('T') {
            if in_time || weeks || tail.is_empty() {
                return Err(invalid("invalid iCalendar DURATION"));
            }
            in_time = true;
            rest = tail;
            continue;
        }
        let length = rest.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 || length == rest.len() {
            return Err(invalid("invalid iCalendar DURATION"));
        }
        let number: i64 = rest[..length]
            .parse()
            .map_err(|_| invalid("duration overflow"))?;
        let unit = rest.as_bytes()[length];
        let (rank, factor) = match (in_time, unit) {
            (false, b'W') if count == 0 => {
                weeks = true;
                (1, 604800)
            }
            (false, b'D') if !weeks => (2, 86400),
            (true, b'H') => (3, 3600),
            (true, b'M') => (4, 60),
            (true, b'S') => (5, 1),
            _ => return Err(invalid("invalid iCalendar DURATION unit")),
        };
        if rank <= last_rank || (weeks && count != 0) {
            return Err(invalid("invalid iCalendar DURATION order"));
        }
        last_rank = rank;
        count += 1;
        if !in_time {
            let factor = if unit == b'W' { 7 } else { 1 };
            days = days
                .checked_add(
                    (number as u64)
                        .checked_mul(factor)
                        .ok_or_else(|| invalid("duration overflow"))?,
                )
                .ok_or_else(|| invalid("duration overflow"))?;
        } else {
            seconds = seconds
                .checked_add(
                    number
                        .checked_mul(factor)
                        .ok_or_else(|| invalid("duration overflow"))?,
                )
                .ok_or_else(|| invalid("duration overflow"))?;
        }
        rest = &rest[length + 1..];
    }
    if count == 0 {
        return Err(invalid("empty iCalendar DURATION"));
    }
    Ok(CalendarDuration { days, seconds })
}

fn duration_end(start: &EventTime, component: &Component<'_>, raw: &str) -> Result<EventTime> {
    let duration = duration(raw)?;
    let seconds =
        Duration::try_seconds(duration.seconds).ok_or_else(|| invalid("duration overflow"))?;
    match start {
        EventTime::Date(date) => {
            if duration.seconds != 0 {
                return Err(invalid("all-day DURATION must contain only days or weeks"));
            }
            Ok(EventTime::Date(
                date.checked_add_days(chrono::Days::new(duration.days))
                    .ok_or_else(|| invalid("date overflow"))?,
            ))
        }
        EventTime::Instant(instant) => {
            let property = component
                .find_prop("DTSTART")
                .ok_or_else(|| invalid("missing DTSTART"))?;
            let end = if let Some(tzid) = parameter(property, "TZID") {
                let tz: chrono_tz::Tz = tzid
                    .parse()
                    .map_err(|_| invalid("unsupported custom timezone"))?;
                instant
                    .with_timezone(&tz)
                    .checked_add_days(chrono::Days::new(duration.days))
                    .map(|dt| dt.with_timezone(&Utc))
            } else {
                instant.checked_add_days(chrono::Days::new(duration.days))
            };
            Ok(EventTime::Instant(
                end.and_then(|dt| dt.checked_add_signed(seconds))
                    .ok_or_else(|| {
                        invalid("duration crosses an ambiguous local time or overflows")
                    })?,
            ))
        }
    }
}

fn event_times(component: &Component<'_>) -> Result<(EventTime, EventTime)> {
    let start = required_time(component, "DTSTART")?;
    let end = if component.find_prop("DTEND").is_some() {
        if component.find_prop("DURATION").is_some() {
            return Err(invalid("VEVENT has both DTEND and DURATION"));
        }
        let end = required_time(component, "DTEND")?;
        if end.instant() <= start.instant() {
            return Err(invalid("DTEND must be later than DTSTART"));
        }
        end
    } else if let Some(d) = value(component, "DURATION") {
        duration_end(&start, component, d)?
    } else if matches!(start, EventTime::Date(_)) {
        start.shifted(Duration::days(1))?
    } else {
        start.clone()
    };
    if std::mem::discriminant(&start) != std::mem::discriminant(&end)
        || end.instant() < start.instant()
    {
        return Err(invalid("invalid VEVENT time interval"));
    }
    Ok((start, end))
}

pub(super) fn event_view(
    component: &Component<'_>,
    calendar: &CalendarCollection,
    href: &str,
    etag: Option<&str>,
) -> Result<CalendarEvent> {
    let (start, end) = event_times(component)?;
    let text = |key| value(component, key).map(super::unescape_ical_text);
    Ok(CalendarEvent {
        calendar_id: calendar.id.clone(),
        calendar_name: calendar.name.clone(),
        href: href.to_string(),
        uid: text("UID"),
        recurrence_id: component
            .find_prop("RECURRENCE-ID")
            .map(property_time)
            .transpose()?
            .map(|t| t.display()),
        summary: text("SUMMARY"),
        description: text("DESCRIPTION"),
        location: text("LOCATION"),
        status: text("STATUS"),
        start: Some(start.display()),
        end: Some(end.display()),
        etag: etag.map(str::to_string),
        all_day: matches!(start, EventTime::Date(_)),
    })
}

pub(super) fn expanded_events(
    input: &str,
    calendar: &CalendarCollection,
    href: &str,
    etag: Option<&str>,
    request: &CalendarEventsRequest,
) -> Result<Vec<CalendarEvent>> {
    let unfolded = parser::unfold(input);
    let doc = document(&unfolded)?;
    let mut events = Vec::new();
    for component in doc.components.iter().filter(|c| c.name == "VEVENT") {
        if ["RRULE", "RDATE", "EXRULE", "EXDATE"]
            .iter()
            .any(|p| component.find_prop(p).is_some())
        {
            return Err(YacliError::UnsupportedOperation(
                "CalDAV server did not expand recurrences; refusing an incomplete event list"
                    .to_string(),
            ));
        }
        if value(component, "STATUS") == Some("CANCELLED") {
            continue;
        }
        let (start, end) = event_times(component)?;
        let overlaps = if start == end {
            start.instant() >= request.from && start.instant() < request.to
        } else {
            start.instant() < request.to && end.instant() > request.from
        };
        if overlaps {
            events.push(event_view(component, calendar, href, etag)?);
        }
    }
    Ok(events)
}

fn replace_property<'a>(component: &mut Component<'a>, property: Property<'a>) {
    component.properties.retain(|p| p.name != property.name);
    component.properties.push(property);
}

fn time_property<'a>(
    name: &'static str,
    time: &EventTime,
    template: &Property<'_>,
) -> Result<Property<'a>> {
    let mut property = Property {
        name: name.into(),
        val: String::new().into(),
        params: Vec::new(),
    };
    match time {
        EventTime::Date(date) => {
            property.val = date.format("%Y%m%d").to_string().into();
            property.params.push(parser::Parameter {
                key: "VALUE".into(),
                val: Some("DATE".into()),
            });
        }
        EventTime::Instant(dt) => {
            if let Some(tzid) = parameter(template, "TZID") {
                let tz: chrono_tz::Tz = tzid
                    .parse()
                    .map_err(|_| invalid("unsupported custom timezone"))?;
                property.val = dt
                    .with_timezone(&tz)
                    .format("%Y%m%dT%H%M%S")
                    .to_string()
                    .into();
                property.params.push(parser::Parameter {
                    key: "TZID".into(),
                    val: Some(tzid.to_string().into()),
                });
            } else {
                property.val = dt.format("%Y%m%dT%H%M%SZ").to_string().into();
            }
        }
    }
    Ok(property)
}

// Refuse an update if serialization would alter any field in the complete tree.
fn serialize_document(doc: &Calendar<'_>) -> Result<String> {
    let body = doc.to_string();
    let unfolded = parser::unfold(&body);
    let decoded = document(&unfolded)?;
    if decoded.properties != doc.properties || decoded.components != doc.components {
        return Err(YacliError::Integrity(
            "iCalendar serialization would change preserved fields; no write performed".to_string(),
        ));
    }
    Ok(body)
}

pub(super) struct EditPlan {
    pub body: String,
    pub before: CalendarEvent,
    pub after: CalendarEvent,
}

/// `expanded` supplies only an occurrence's effective times, never its stored object.
pub(super) fn plan_update(
    input: &str,
    expanded: Option<&CalendarEvent>,
    request: &CalendarUpdateRequest,
    calendar: &CalendarCollection,
    href: &str,
    etag: &str,
    account: &str,
) -> Result<EditPlan> {
    let unfolded = parser::unfold(input);
    let mut doc = document(&unfolded)?;
    let target_index = select_update_target(&mut doc, expanded, request)?;
    let target = &mut doc.components[target_index];
    authorize_update(target, account)?;
    let before = event_view(target, calendar, href, Some(etag))?;
    apply_update_fields(target, request)?;
    advance_revision(target)?;
    let after = event_view(target, calendar, href, Some(etag))?;
    Ok(EditPlan {
        body: serialize_document(&doc)?,
        before,
        after,
    })
}

fn select_update_target(
    doc: &mut Calendar<'_>,
    expanded: Option<&CalendarEvent>,
    request: &CalendarUpdateRequest,
) -> Result<usize> {
    let matching = doc
        .components
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name == "VEVENT" && uid(c).as_deref() == Some(&request.uid))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Err(invalid("UID not present in fetched calendar object"));
    }
    if doc
        .components
        .iter()
        .any(|c| c.name == "VEVENT" && uid(c).as_deref() != Some(&request.uid))
    {
        return Err(invalid("calendar object contains unrelated UIDs"));
    }
    if matching.iter().any(|&i| {
        doc.components[i]
            .find_prop("RECURRENCE-ID")
            .is_some_and(|p| parameter(p, "RANGE").is_some())
    }) {
        return Err(YacliError::UnsupportedOperation(
            "RANGE recurrence exceptions cannot be updated safely".to_string(),
        ));
    }
    let masters = matching
        .iter()
        .copied()
        .filter(|&i| doc.components[i].find_prop("RECURRENCE-ID").is_none())
        .collect::<Vec<_>>();
    if masters.len() != 1 {
        return Err(invalid("expected exactly one master VEVENT for UID"));
    }
    let master_index = masters[0];
    let recurring = matching.len() > 1
        || ["RRULE", "RDATE", "EXRULE", "EXDATE"]
            .iter()
            .any(|p| doc.components[master_index].find_prop(p).is_some());
    let occurrence = request
        .occurrence
        .as_deref()
        .map(EventTime::input)
        .transpose()?;
    let target_index = if request.scope == "event" && !recurring && occurrence.is_none() {
        master_index
    } else if request.scope == "occurrence" && recurring {
        let occurrence = occurrence
            .as_ref()
            .ok_or_else(|| invalid("occurrence scope requires --occurrence"))?;
        let mut detached = Vec::new();
        for &i in &matching {
            if let Some(rid) = doc.components[i].find_prop("RECURRENCE-ID")
                && property_time(rid)? == *occurrence
            {
                detached.push(i);
            }
        }
        if detached.len() > 1 {
            return Err(invalid("duplicate recurrence identity"));
        }
        if let Some(i) = detached.first() {
            *i
        } else {
            let effective = expanded.ok_or_else(|| {
                invalid("occurrence not found in server expansion (possibly excluded or cancelled)")
            })?;
            if effective.uid.as_deref() != Some(&request.uid)
                || effective.recurrence_id.as_deref() != Some(occurrence.display().as_str())
            {
                return Err(invalid("server expansion returned a different occurrence"));
            }
            let mut instance = doc.components[master_index].clone();
            instance
                .properties
                .retain(|p| !["RRULE", "EXRULE", "RDATE", "EXDATE"].contains(&p.name.as_str()));
            let template: Property<'static> = icalendar::Property::from(
                instance
                    .find_prop("DTSTART")
                    .ok_or_else(|| invalid("missing DTSTART"))?
                    .clone(),
            )
            .into();
            if std::mem::discriminant(&property_time(&template)?)
                != std::mem::discriminant(occurrence)
            {
                return Err(invalid("occurrence value type differs from master"));
            }
            replace_property(
                &mut instance,
                time_property("RECURRENCE-ID", occurrence, &template)?,
            );
            replace_property(
                &mut instance,
                time_property(
                    "DTSTART",
                    &EventTime::input(
                        effective
                            .start
                            .as_deref()
                            .ok_or_else(|| invalid("expanded DTSTART missing"))?,
                    )?,
                    &template,
                )?,
            );
            let start = EventTime::input(
                effective
                    .start
                    .as_deref()
                    .ok_or_else(|| invalid("expanded DTSTART missing"))?,
            )?;
            let end = EventTime::input(
                effective
                    .end
                    .as_deref()
                    .ok_or_else(|| invalid("expanded DTEND missing"))?,
            )?;
            instance.properties.retain(|p| p.name != "DTEND");
            if instance.find_prop("DURATION").is_some() {
                if event_times(&instance)?.1 != end {
                    return Err(invalid("expanded duration differs from master duration"));
                }
            } else if end != start {
                replace_property(&mut instance, time_property("DTEND", &end, &template)?);
            }
            doc.components.push(instance);
            doc.components.len() - 1
        }
    } else {
        return Err(invalid(
            "use scope=event for a standalone event, or scope=occurrence with an original recurrence id; series updates are unsupported",
        ));
    };
    Ok(target_index)
}

fn authorize_update(target: &Component<'_>, account: &str) -> Result<()> {
    if value(target, "STATUS") == Some("CANCELLED") {
        return Err(invalid("cannot update a cancelled occurrence"));
    }
    if let Some(organizer) = value(target, "ORGANIZER") {
        let organizer = organizer
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("mailto:"))
            .map(|_| &organizer[7..])
            .unwrap_or(organizer);
        if !organizer.eq_ignore_ascii_case(account) {
            return Err(YacliError::UnsupportedOperation(
                "rescheduling another organizer's event is unsupported; use the organizer account"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn apply_update_fields(target: &mut Component<'_>, request: &CalendarUpdateRequest) -> Result<()> {
    let (old_start, old_end) = event_times(target)?;
    let start = request
        .start
        .as_deref()
        .map(EventTime::input)
        .transpose()?
        .unwrap_or(old_start.clone());
    let end = if let Some(end) = request.end.as_deref() {
        EventTime::input(end)?
    } else if let Some(raw) = value(target, "DURATION") {
        duration_end(&start, target, raw)?
    } else {
        start.shifted(old_end.instant() - old_start.instant())?
    };
    let point_event =
        matches!(old_start, EventTime::Instant(_)) && old_start == old_end && request.end.is_none();
    if std::mem::discriminant(&old_start) != std::mem::discriminant(&start)
        || std::mem::discriminant(&start) != std::mem::discriminant(&end)
        || (end.instant() <= start.instant() && !point_event)
    {
        return Err(invalid(
            "update must preserve date/time value type and a positive duration (or an instantaneous event)",
        ));
    }
    if request.start.is_some() || request.end.is_some() {
        let template: Property<'static> = icalendar::Property::from(
            target
                .find_prop("DTSTART")
                .ok_or_else(|| invalid("missing DTSTART"))?
                .clone(),
        )
        .into();
        replace_property(target, time_property("DTSTART", &start, &template)?);
        let keep_duration = request.end.is_none() && target.find_prop("DURATION").is_some();
        if point_event || keep_duration {
            target.properties.retain(|p| p.name != "DTEND");
        } else {
            replace_property(target, time_property("DTEND", &end, &template)?);
        }
        if !keep_duration {
            target.properties.retain(|p| p.name != "DURATION");
        }
    }
    for (name, supplied) in [
        ("SUMMARY", &request.summary),
        ("DESCRIPTION", &request.description),
        ("LOCATION", &request.location),
    ] {
        if let Some(text) = supplied {
            replace_property(
                target,
                Property {
                    name: name.into(),
                    val: super::escape_ical_text(text).into(),
                    params: Vec::new(),
                },
            );
        }
    }
    Ok(())
}

fn advance_revision(target: &mut Component<'_>) -> Result<()> {
    let sequence: u32 = value(target, "SEQUENCE")
        .unwrap_or("0")
        .parse()
        .map_err(|_| invalid("invalid SEQUENCE"))?;
    let sequence = sequence
        .checked_add(1)
        .ok_or_else(|| invalid("SEQUENCE overflow"))?;
    replace_property(
        target,
        Property {
            name: "SEQUENCE".into(),
            val: sequence.to_string().into(),
            params: Vec::new(),
        },
    );
    let now = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for name in ["DTSTAMP", "LAST-MODIFIED"] {
        replace_property(
            target,
            Property {
                name: name.into(),
                val: now.clone().into(),
                params: Vec::new(),
            },
        );
    }
    Ok(())
}

pub(super) fn resource_has_uid(input: &str, uid: &str) -> Result<bool> {
    let unfolded = parser::unfold(input);
    let doc = document(&unfolded)?;
    Ok(doc
        .components
        .iter()
        .any(|c| c.name == "VEVENT" && self::uid(c).as_deref() == Some(uid)))
}

/// Check every submitted component/property survives server storage. Server-managed
/// revision metadata and scheduling delivery status may change independently.
pub(super) fn verify_saved_document(expected: &str, actual: &str) -> Result<()> {
    fn protected(property: &Property<'_>) -> Option<String> {
        if [
            "DTSTAMP",
            "LAST-MODIFIED",
            "CREATED",
            "SEQUENCE",
            "DTSTART",
            "DTEND",
            "DURATION",
            "RECURRENCE-ID",
        ]
        .contains(&property.name.as_str())
        {
            return None;
        }
        let mut property = property.clone();
        property.params.retain(|p| p.key != "SCHEDULE-STATUS");
        // Parameter order is not significant.
        property
            .params
            .sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        Some(property.to_string())
    }
    fn properties_survive(expected: &[Property<'_>], actual: &[Property<'_>]) -> bool {
        let mut actual = actual.iter().filter_map(protected).collect::<Vec<_>>();
        for property in expected.iter().filter_map(protected) {
            if let Some(index) = actual.iter().position(|p| p == &property) {
                actual.remove(index);
            } else {
                return false;
            }
        }
        true
    }
    fn same_identity(left: &Component<'_>, right: &Component<'_>) -> Result<bool> {
        if left.name != right.name {
            return Ok(false);
        }
        if left.name == "VEVENT" {
            return Ok(uid(left) == uid(right)
                && left
                    .find_prop("RECURRENCE-ID")
                    .map(property_time)
                    .transpose()?
                    == right
                        .find_prop("RECURRENCE-ID")
                        .map(property_time)
                        .transpose()?);
        }
        Ok(left.name != "VTIMEZONE" || value(left, "TZID") == value(right, "TZID"))
    }
    fn components_survive(expected: &[Component<'_>], actual: &[Component<'_>]) -> Result<bool> {
        let mut used = std::collections::BTreeSet::new();
        for left in expected {
            let mut found = false;
            for (index, right) in actual.iter().enumerate() {
                if used.contains(&index) || !same_identity(left, right)? {
                    continue;
                }
                if left.name == "VEVENT" && event_times(left)? != event_times(right)? {
                    continue;
                }
                if properties_survive(&left.properties, &right.properties)
                    && components_survive(&left.components, &right.components)?
                {
                    used.insert(index);
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(false);
            }
        }
        Ok(true)
    }
    let expected = parser::unfold(expected);
    let expected = document(&expected)?;
    let actual = parser::unfold(actual);
    let actual = document(&actual)?;
    if !properties_survive(&expected.properties, &actual.properties)
        || !components_survive(&expected.components, &actual.components)?
    {
        return Err(YacliError::Integrity(
            "saved calendar object lost or changed preserved fields".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn has_detached(input: &str, uid: &str, occurrence: &str) -> Result<bool> {
    let unfolded = parser::unfold(input);
    let doc = document(&unfolded)?;
    let occurrence = EventTime::input(occurrence)?;
    for event in doc
        .components
        .iter()
        .filter(|c| c.name == "VEVENT" && self::uid(c).as_deref() == Some(uid))
    {
        if let Some(rid) = event.find_prop("RECURRENCE-ID")
            && property_time(rid)? == occurrence
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn verified_event(
    input: &str,
    expected: &CalendarEvent,
    calendar: &CalendarCollection,
    etag: &str,
) -> Result<CalendarEvent> {
    let unfolded = parser::unfold(input);
    let doc = document(&unfolded)?;
    let mut matching = Vec::new();
    for event in doc
        .components
        .iter()
        .filter(|c| c.name == "VEVENT" && uid(c).as_deref() == expected.uid.as_deref())
    {
        let rid = event
            .find_prop("RECURRENCE-ID")
            .map(property_time)
            .transpose()?
            .map(|t| t.display());
        if rid == expected.recurrence_id {
            matching.push(event_view(event, calendar, &expected.href, Some(etag))?);
        }
    }
    if matching.len() == 1 {
        let actual = matching.remove(0);
        if actual.start == expected.start
            && actual.end == expected.end
            && actual.summary == expected.summary
            && actual.description == expected.description
            && actual.location == expected.location
            && actual.status == expected.status
        {
            return Ok(actual);
        }
    }
    Err(YacliError::Integrity(
        "calendar update could not be verified; read the event before retrying".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calendar() -> CalendarCollection {
        CalendarCollection {
            id: "team".into(),
            name: "Team".into(),
            href: "/cal/team/".into(),
            description: None,
        }
    }
    fn request() -> CalendarUpdateRequest {
        CalendarUpdateRequest {
            calendar: "team".into(),
            uid: "series".into(),
            scope: "occurrence".into(),
            occurrence: Some("2026-10-20T07:30:00Z".into()),
            start: Some("2026-10-27T07:30:00Z".into()),
            end: None,
            summary: None,
            description: None,
            location: None,
            if_match: None,
        }
    }
    const SERIES: &str = include_str!("../../tests/fixtures/calendar/series.ics");

    fn effective() -> CalendarEvent {
        CalendarEvent {
            calendar_id: "team".into(),
            calendar_name: "Team".into(),
            href: "/cal/team/one.ics".into(),
            uid: Some("series".into()),
            recurrence_id: Some("2026-10-20T07:30:00Z".into()),
            summary: Some("Weekly".into()),
            start: Some("2026-10-20T07:30:00Z".into()),
            end: Some("2026-10-20T08:00:00Z".into()),
            description: None,
            location: None,
            status: None,
            etag: Some("\"v1\"".into()),
            all_day: false,
        }
    }

    fn plan(
        input: &str,
        expanded: Option<&CalendarEvent>,
        req: &CalendarUpdateRequest,
    ) -> Result<EditPlan> {
        plan_update(
            input,
            expanded,
            req,
            &calendar(),
            "/cal/team/one.ics",
            "\"v1\"",
            "owner@example.test",
        )
    }

    #[test]
    fn move_generated_occurrence_preserves_full_tree_and_original_identity() {
        let result = plan(SERIES, Some(&effective()), &request()).unwrap();
        let before_text = parser::unfold(SERIES);
        let before = document(&before_text).unwrap();
        let after_text = parser::unfold(&result.body);
        let after = document(&after_text).unwrap();
        assert_eq!(before.properties, after.properties);
        assert_eq!(
            before.components,
            after.components[..before.components.len()]
        );
        let target = after.components.last().unwrap();
        assert_eq!(value(target, "RECURRENCE-ID"), Some("20261020T113000"));
        assert_eq!(value(target, "DTSTART"), Some("20261027T113000"));
        assert_eq!(value(target, "DTEND"), Some("20261027T120000"));
        assert_eq!(target.components, before.components[1].components);
        assert_eq!(
            target
                .properties
                .iter()
                .filter(|p| p.name == "ATTENDEE")
                .count(),
            2
        );
        assert_eq!(
            value(target, "X-VENDOR"),
            Some("a-long-value-that-is-folded-onto-the-next-line")
        );
        assert!(target.find_prop("RRULE").is_none());
        assert_eq!(result.after.recurrence_id, effective().recurrence_id);
        assert_eq!(result.before.start, effective().start);
    }

    #[test]
    fn repeated_move_updates_existing_exception_without_creating_another() {
        let first = plan(SERIES, Some(&effective()), &request()).unwrap();
        let mut req = request();
        req.start = Some("2026-10-28T08:00:00Z".into());
        let next = plan(&first.body, None, &req).unwrap();
        assert_eq!(next.body.matches("RECURRENCE-ID").count(), 1);
        assert_eq!(next.before.start.as_deref(), Some("2026-10-27T07:30:00Z"));
        assert_eq!(next.after.end.as_deref(), Some("2026-10-28T08:30:00Z"));
    }

    #[test]
    fn update_rejects_implicit_series_foreign_organizer_and_missing_instance() {
        let mut req = request();
        req.scope = "event".into();
        req.occurrence = None;
        assert!(plan(SERIES, None, &req).is_err());
        assert!(plan(SERIES, None, &request()).is_err());
        assert!(
            plan_update(
                SERIES,
                Some(&effective()),
                &request(),
                &calendar(),
                "/cal/team/one.ics",
                "\"v1\"",
                "guest@example.test"
            )
            .is_err()
        );
    }

    #[test]
    fn all_day_move_preserves_exclusive_end_and_rejects_value_type_change() {
        let input = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:series\nDTSTART;VALUE=DATE:20261020\nDTEND;VALUE=DATE:20261022\nEND:VEVENT\nEND:VCALENDAR\n";
        let mut req = request();
        req.scope = "event".into();
        req.occurrence = None;
        req.start = Some("2026-10-27".into());
        let result = plan(input, None, &req).unwrap();
        assert_eq!(result.after.end.as_deref(), Some("2026-10-29"));
        assert!(result.after.all_day);
        req.start = Some("2026-10-27T00:00:00Z".into());
        assert!(plan(input, None, &req).is_err());
    }

    #[test]
    fn duration_event_preserves_elapsed_duration_across_dst() {
        let input = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:series\nDTSTART;TZID=Europe/Berlin:20261024T090000\nDURATION:PT30M\nEND:VEVENT\nEND:VCALENDAR\n";
        let mut req = request();
        req.scope = "event".into();
        req.occurrence = None;
        req.start = Some("2026-10-26T09:00:00+01:00".into());
        let result = plan(input, None, &req).unwrap();
        assert_eq!(result.before.start.as_deref(), Some("2026-10-24T07:00:00Z"));
        assert_eq!(result.after.end.as_deref(), Some("2026-10-26T08:30:00Z"));
        assert!(
            result
                .body
                .contains("DTSTART;TZID=Europe/Berlin:20261026T090000")
        );
        assert!(result.body.contains("DURATION:PT30M"));
    }

    #[test]
    fn invalid_floating_ambiguous_and_malformed_times_fail_explicitly() {
        for text in [
            "DTSTART:20261020T090000",
            "DTSTART;TZID=Europe/Berlin:20261025T023000",
            "DTSTART;TZID=Unknown/Zone:20261020T090000",
        ] {
            assert!(property_time(&Property::try_from(text).unwrap()).is_err());
        }
        assert!(document("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:broken\nEND:VCALENDAR\n").is_err());
        for invalid in [
            "P",
            "PT",
            "P1M",
            "-PT1H",
            "P1W1D",
            "PT1S1H",
            "P1DT",
            "PT999999999999999999999S",
        ] {
            assert!(duration(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn point_events_and_exclusive_boundaries_follow_half_open_window() {
        let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:point\nDTSTART:20261020T090000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:ending\nDTSTART:20261020T080000Z\nDTEND:20261020T090000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:next\nDTSTART:20261020T100000Z\nEND:VEVENT\nEND:VCALENDAR\n";
        let req = CalendarEventsRequest {
            calendar: "team".into(),
            from: EventTime::input("2026-10-20T09:00:00Z").unwrap().instant(),
            to: EventTime::input("2026-10-20T10:00:00Z").unwrap().instant(),
            limit: 100,
        };
        let events = expanded_events(input, &calendar(), "/cal/team/one.ics", None, &req).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid.as_deref(), Some("point"));
        assert!(expanded_events(SERIES, &calendar(), "/cal/team/one.ics", None, &req).is_err());
    }
    #[test]
    fn quoted_commas_and_multi_address_parameters_preserve_their_distinct_meanings() {
        let input = SERIES.replace("CN=\"Guest: One; Two\"", "CN=\"Doe, John\";MEMBER=\"mailto:a@example.test\",\"mailto:b@example.test\";X-LIST=one,two");
        let result = plan(&input, Some(&effective()), &request()).unwrap();
        let body = parser::unfold(&result.body);
        assert_eq!(body.matches("CN=\"Doe, John\"").count(), 2);
        assert_eq!(
            body.matches("MEMBER=\"mailto:a@example.test\",\"mailto:b@example.test\"")
                .count(),
            2
        );
        verify_saved_document(&result.body, &result.body).unwrap();
    }

    #[test]
    fn nominal_day_duration_respects_dst_and_survives_a_move() {
        let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:series\nDTSTART;TZID=Europe/Berlin:20261024T090000\nDURATION:P1D\nEND:VEVENT\nEND:VCALENDAR\n";
        let mut req = request();
        req.scope = "event".into();
        req.occurrence = None;
        req.start = Some("2026-10-26T09:00:00+01:00".into());
        let result = plan(input, None, &req).unwrap();
        assert_eq!(result.before.end.as_deref(), Some("2026-10-25T08:00:00Z"));
        assert_eq!(result.after.end.as_deref(), Some("2026-10-27T08:00:00Z"));
        assert!(result.body.contains("DURATION:P1D"));
    }

    #[test]
    fn move_of_point_event_does_not_add_an_equal_dtend() {
        let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:series\nDTSTART:20261020T073000Z\nEND:VEVENT\nEND:VCALENDAR\n";
        let mut req = request();
        req.scope = "event".into();
        req.occurrence = None;
        let result = plan(input, None, &req).unwrap();
        assert_eq!(result.after.start, result.after.end);
        assert!(!result.body.contains("DTEND"));
    }

    #[test]
    fn expanded_window_filters_moved_out_cancelled_and_keeps_distinct_instances() {
        let input = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:series\nRECURRENCE-ID:20261013T090000Z\nDTSTART:20261020T090000Z\nDTEND:20261020T100000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:series\nRECURRENCE-ID:20261020T090000Z\nDTSTART:20261027T090000Z\nDTEND:20261027T100000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:series\nRECURRENCE-ID:20261021T090000Z\nDTSTART:20261021T090000Z\nSTATUS:CANCELLED\nEND:VEVENT\nBEGIN:VEVENT\nUID:series\nRECURRENCE-ID:20261022T090000Z\nDTSTART:20261022T090000Z\nEND:VEVENT\nEND:VCALENDAR\n";
        let req = CalendarEventsRequest {
            calendar: "team".into(),
            from: EventTime::input("2026-10-19T00:00:00Z").unwrap().instant(),
            to: EventTime::input("2026-10-26T00:00:00Z").unwrap().instant(),
            limit: 100,
        };
        let events = expanded_events(input, &calendar(), "/cal/team/item.ics", None, &req).unwrap();
        assert_eq!(events.len(), 2);
        assert_ne!(events[0].recurrence_id, events[1].recurrence_id);
        assert_eq!(
            events[0].recurrence_id.as_deref(),
            Some("2026-10-13T09:00:00Z")
        );
    }
}
