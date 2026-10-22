use mxbot_common::config::TimeOfDay;
pub use mxbot_common::config::{MatrixConfig, SecurityConfig};
use mxbot_common::directory::DirectoryConfig;
use serde::{Deserialize, Deserializer};

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
    /// If set, one resident's Matrix accounts count as one person (src/directory.rs).
    pub directory: Option<DirectoryConfig>,
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
    /// Default: "09:00". A malformed time is refused at startup.
    #[serde(default = "default_reminder_time")]
    pub reminder_time: TimeOfDay,
    /// IANA timezone used for weekday calculations (e.g. "Europe/Berlin").
    /// Default: UTC. An unknown name is refused at startup.
    #[serde(
        default = "default_timezone",
        deserialize_with = "deserialize_timezone"
    )]
    pub timezone: chrono_tz::Tz,
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
    /// token that grants access. Overridden by the environment variable
    /// [`ICAL_PUBLIC_URL_ENV`] if it is set.
    pub public_url: String,
}

/// Environment variable that overrides `[ical_server] public_url`, so the
/// deployment can set the public address (e.g. after a domain change) without
/// editing the config.
pub const ICAL_PUBLIC_URL_ENV: &str = "CLEANING_ICAL_PUBLIC_URL";

impl ICalServerConfig {
    /// Uses `value` as `public_url` if it is set and not blank.
    pub fn apply_public_url_override(&mut self, value: Option<String>) {
        if let Some(url) = value.map(|v| v.trim().to_owned()) {
            if !url.is_empty() {
                self.public_url = url;
            }
        }
    }
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
fn default_reminder_time() -> TimeOfDay {
    TimeOfDay::new(9, 0)
}
fn default_timezone() -> chrono_tz::Tz {
    chrono_tz::UTC
}

/// An IANA timezone name such as "Europe/Berlin".
fn deserialize_timezone<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<chrono_tz::Tz, D::Error> {
    let name = String::deserialize(deserializer)?;
    name.trim().parse().map_err(|_| {
        serde::de::Error::custom(format!(
            "unknown timezone {name:?} (expected an IANA name such as \"Europe/Berlin\")"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_parses_and_passes_the_startup_checks() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let s = &config.schedule;
        assert_eq!(s.timezone, chrono_tz::Europe::Berlin);
        assert_eq!(s.reminder_time, TimeOfDay::new(9, 0));
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
        let directory = include_str!("../config.example.toml")
            .replace("# [directory]", "[directory]")
            .replace("# provider", "provider")
            .replace("# api_url", "api_url")
            .replace("# server_name", "server_name");
        let d = toml::from_str::<Config>(&directory)
            .unwrap()
            .directory
            .unwrap();
        assert_eq!(
            d.provider,
            mxbot_common::directory::DirectoryProvider::Authentik
        );
        assert_eq!(d.matrix_ids_attribute, "matrix_ids");
        assert_eq!(d.token_env, "DIRECTORY_TOKEN");
        assert_eq!(d.refresh_minutes, 15);
    }

    /// The config house-infra renders today (stacks/matrix-bots/secrets/
    /// cleaning-bot.config.toml.j2, with placeholder secrets) still loads.
    #[test]
    fn the_deployed_config_parses() {
        let config: Config = toml::from_str(include_str!("../tests/deployed.config.toml")).unwrap();
        let s = &config.schedule;
        assert_eq!(s.timezone, chrono_tz::Europe::Berlin);
        assert_eq!(s.reminder_time, TimeOfDay::new(9, 0));
        assert_eq!(s.final_reminder_weekday, 6);
        assert!(config.ical_server.is_some() && config.directory.is_some());
        config.security.validate().unwrap();
    }

    fn schedule(extra: &str) -> Result<ScheduleConfig, toml::de::Error> {
        toml::from_str(&format!("room_id = \"!room:example.org\"\n{extra}"))
    }

    #[test]
    fn reminder_time_and_timezone_are_checked_at_load() {
        let s = schedule("").unwrap();
        assert_eq!(s.timezone, chrono_tz::UTC);
        assert_eq!(s.reminder_time, TimeOfDay::new(9, 0));
        let s = schedule("reminder_time = \"18:30\"\ntimezone = \"America/New_York\"").unwrap();
        assert_eq!(s.reminder_time, TimeOfDay::new(18, 30));
        assert_eq!(s.timezone, chrono_tz::America::New_York);
        for bad in ["9", "25:00", "09:60", "nine", ""] {
            let Err(e) = schedule(&format!("reminder_time = \"{bad}\"")) else {
                panic!("{bad} was accepted");
            };
            assert!(e.to_string().contains("time"), "{bad}: {e}");
        }
        for bad in ["Europe/Berlinn", "CEST", ""] {
            let Err(e) = schedule(&format!("timezone = \"{bad}\"")) else {
                panic!("{bad} was accepted");
            };
            assert!(e.to_string().contains("unknown timezone"), "{bad}: {e}");
        }
    }

    #[test]
    fn ical_public_url_override() {
        let mut ical = ICalServerConfig {
            bind_addr: "0.0.0.0:8080".to_owned(),
            public_url: "https://old.example.net".to_owned(),
        };
        ical.apply_public_url_override(None);
        assert_eq!(ical.public_url, "https://old.example.net");
        ical.apply_public_url_override(Some(" ".to_owned()));
        assert_eq!(ical.public_url, "https://old.example.net");
        ical.apply_public_url_override(Some("https://cal.example.org".to_owned()));
        assert_eq!(ical.public_url, "https://cal.example.org");
    }
}
