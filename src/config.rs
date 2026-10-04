pub use mxbot_common::config::{MatrixConfig, SecurityConfig};
use serde::Deserialize;

/// Strategy used by the slot resolver to fill empty assignments.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FillStrategy {
    #[default]
    RoundRobin,
    LeastLoadedFirst,
}

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    pub schedule: ScheduleConfig,
    /// If set, enables the HTTP iCal feed server.
    pub ical_server: Option<ICalServerConfig>,
}

#[derive(Deserialize)]
pub struct ScheduleConfig {
    /// Matrix room ID where reminders and replies are posted.
    pub room_id: String,
    /// Default rhythm of new groups, in weeks (1 = every week). Groups from
    /// before per-group rhythms adopt it once at startup.
    #[serde(default = "default_interval_weeks")]
    pub interval_weeks: u32,
    /// Weekday to send the initial reminder (0 = Mon … 6 = Sun).
    #[serde(default = "default_reminder_weekday")]
    pub reminder_weekday: u8,
    /// Weekday to send the final "not done yet" reminder for whole-week
    /// turns (shifts get theirs on their last day).
    #[serde(default = "default_final_reminder_weekday")]
    pub final_reminder_weekday: u8,
    /// Local time (HH:MM) at or after which reminders are allowed to fire.
    /// Default: "09:00".
    #[serde(default = "default_reminder_time")]
    pub reminder_time: String,
    /// IANA timezone string used for weekday calculations (e.g. "Europe/Berlin").
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Assignment fill strategy.
    #[serde(default)]
    pub fill_strategy: FillStrategy,
    /// How many due weeks per group to pre-materialize assignments for
    /// (1–104, checked at startup).
    #[serde(default = "default_materialize_weeks")]
    pub materialize_weeks: u32,
}

/// Optional HTTP server that serves per-person iCal feeds.
/// If absent the `!ical` command falls back to Matrix file upload.
#[derive(Deserialize, Clone)]
pub struct ICalServerConfig {
    /// Address to bind the HTTP server to, e.g. "0.0.0.0:8080".
    pub bind_addr: String,
    /// Public base URL shown to users, e.g. "https://cal.example.org" (a
    /// trailing slash is ignored). Should be HTTPS: feed URLs carry the
    /// token that grants access.
    pub public_url: String,
}

fn default_interval_weeks() -> u32 {
    1
}
fn default_materialize_weeks() -> u32 {
    26
}
fn default_reminder_weekday() -> u8 {
    0
} // Monday
fn default_final_reminder_weekday() -> u8 {
    6
} // Sunday
fn default_reminder_time() -> String {
    "09:00".to_owned()
}
fn default_timezone() -> String {
    "UTC".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_parses_and_passes_the_startup_checks() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let s = &config.schedule;
        assert!(s.timezone.parse::<chrono_tz::Tz>().is_ok());
        assert!((1..=52).contains(&s.interval_weeks));
        assert!(s.reminder_weekday < 7 && s.final_reminder_weekday < 7);
        assert_eq!(s.fill_strategy, FillStrategy::RoundRobin);
        assert!((1..=104).contains(&s.materialize_weeks));
        // With the commented-out section filled in, it reads too.
        let ical = include_str!("../config.example.toml")
            .replace("# [ical_server]", "[ical_server]")
            .replace("# bind_addr", "bind_addr")
            .replace("# public_url", "public_url");
        let config: Config = toml::from_str(&ical).unwrap();
        assert!(config
            .ical_server
            .unwrap()
            .public_url
            .starts_with("https://"));
    }
}
