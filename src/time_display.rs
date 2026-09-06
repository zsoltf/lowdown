use chrono::{DateTime, FixedOffset, Local, Utc};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TimeDisplayMode {
    Local,
    Session,
    Utc,
}

impl TimeDisplayMode {
    pub(crate) fn from_env_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "session" => Some(Self::Session),
            "utc" => Some(Self::Utc),
            _ => None,
        }
    }
}

pub(crate) fn current_mode() -> TimeDisplayMode {
    std::env::var("LOWDOWN_TIMEZONE")
        .ok()
        .and_then(|raw| TimeDisplayMode::from_env_value(&raw))
        .unwrap_or(TimeDisplayMode::Local)
}

pub(crate) fn format_short_time(value: DateTime<FixedOffset>) -> String {
    match current_mode() {
        TimeDisplayMode::Local => value.with_timezone(&Local).format("%H:%M").to_string(),
        TimeDisplayMode::Session => value.format("%H:%M").to_string(),
        TimeDisplayMode::Utc => value.with_timezone(&Utc).format("%H:%M").to_string(),
    }
}

pub(crate) fn format_digest_datetime(value: DateTime<FixedOffset>) -> String {
    match current_mode() {
        TimeDisplayMode::Local => value
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M %Z")
            .to_string(),
        TimeDisplayMode::Session => value.format("%Y-%m-%d %H:%M %:z").to_string(),
        TimeDisplayMode::Utc => value
            .with_timezone(&Utc)
            .format("%Y-%m-%d %H:%M UTC")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_time_modes() {
        assert_eq!(
            TimeDisplayMode::from_env_value("local"),
            Some(TimeDisplayMode::Local)
        );
        assert_eq!(
            TimeDisplayMode::from_env_value("session"),
            Some(TimeDisplayMode::Session)
        );
        assert_eq!(
            TimeDisplayMode::from_env_value("utc"),
            Some(TimeDisplayMode::Utc)
        );
        assert_eq!(TimeDisplayMode::from_env_value("weird"), None);
    }

    #[test]
    fn session_and_utc_formatters_are_deterministic() {
        let timestamp =
            DateTime::parse_from_rfc3339("2026-04-16T12:34:56-07:00").expect("timestamp");
        assert_eq!(
            match TimeDisplayMode::Session {
                TimeDisplayMode::Local => unreachable!(),
                TimeDisplayMode::Session => timestamp.format("%H:%M").to_string(),
                TimeDisplayMode::Utc => unreachable!(),
            },
            "12:34"
        );
        assert_eq!(
            timestamp.with_timezone(&Utc).format("%H:%M").to_string(),
            "19:34"
        );
    }
}
