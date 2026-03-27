use std::time::Duration;

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn format_channel_points(points: i64) -> String {
    let value = points.abs();
    match value {
        1_000_000.. => format_points_with_suffix(value as f64, 1_000_000.0, "M"),
        1_000.. => format_points_with_suffix(value as f64, 1_000.0, "k"),
        _ => value.to_string(),
    }
}

#[must_use]
pub fn format_points_with_suffix(points: f64, divisor: f64, suffix: &str) -> String {
    let formatted = trim_trailing_zeros(&format!("{:.2}", points / divisor));
    format!("{formatted}{suffix}")
}

#[must_use]
pub fn format_drop_progress(current: i64, required: i64) -> String {
    if required > 0 {
        format!("{current}/{required}")
    } else {
        current.to_string()
    }
}

#[must_use]
pub fn progress_percent(current: i64, required: i64) -> i64 {
    if required <= 0 {
        return if current > 0 { 100 } else { 0 };
    }
    ((current * 100) / required).max(0)
}

#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    let days = seconds / 86_400;
    seconds -= days * 86_400;
    let hours = seconds / 3_600;
    seconds -= hours * 3_600;
    let minutes = seconds / 60;
    seconds -= minutes * 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 || !parts.is_empty() {
        parts.push(format!("{hours:02}h"));
    }
    if minutes > 0 || !parts.is_empty() {
        parts.push(format!("{minutes:02}m"));
    }
    parts.push(format!("{seconds:02}s"));
    parts.join(" ")
}

#[must_use]
pub(crate) fn trim_trailing_zeros(value: &str) -> String {
    value
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_points() {
        assert_eq!(format_channel_points(999), "999");
        assert_eq!(format_channel_points(1_500), "1.5k");
        assert_eq!(format_channel_points(1_500_000), "1.5M");
        assert_eq!(
            format_points_with_suffix(1_250_000.0, 1_000_000.0, "M"),
            "1.25M"
        );
    }

    #[test]
    fn formats_progress() {
        assert_eq!(format_drop_progress(3, 10), "3/10");
        assert_eq!(format_drop_progress(5, 0), "5");
        assert_eq!(progress_percent(5, 10), 50);
        assert_eq!(progress_percent(1, 0), 100);
        assert_eq!(progress_percent(0, 0), 0);
    }

    #[test]
    fn formats_duration_like_go() {
        let duration = Duration::from_secs(24 * 3600 + 2 * 3600 + 3 * 60 + 4);
        assert_eq!(format_duration(duration), "1d 02h 03m 04s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
    }
}
