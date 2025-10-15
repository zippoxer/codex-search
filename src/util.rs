use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

const DISPLAY_DATE: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]");
const DISPLAY_TIME: &[time::format_description::FormatItem<'static>] =
    format_description!("[hour]:[minute]");

pub fn format_timestamp(dt: OffsetDateTime) -> String {
    dt.to_offset(local_offset())
        .format(DISPLAY_DATE)
        .unwrap_or_else(|_| dt.to_string())
}

pub fn format_time_of_day(dt: OffsetDateTime) -> String {
    dt.to_offset(local_offset())
        .format(DISPLAY_TIME)
        .unwrap_or_else(|_| dt.to_string())
}

pub fn format_relative(dt: OffsetDateTime, reference: OffsetDateTime) -> String {
    let diff = reference - dt;
    if diff.is_negative() {
        return "just now".into();
    }

    let secs = diff.whole_seconds();
    if secs <= 0 {
        return "just now".into();
    }

    let minutes = secs / 60;
    if minutes == 0 {
        return format!("{}s ago", secs);
    }

    let hours = minutes / 60;
    if hours == 0 {
        return format!("{}m ago", minutes);
    }

    let days = hours / 24;
    if days == 0 {
        let rem_minutes = minutes % 60;
        if rem_minutes == 0 {
            return format!("{}h ago", hours);
        }
        return format!("{}h {}m ago", hours, rem_minutes);
    }

    let rem_hours = hours % 24;
    if rem_hours == 0 {
        return format!("{}d ago", days);
    }

    format!("{}d {}h ago", days, rem_hours)
}

fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}
