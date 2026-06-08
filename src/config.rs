use serde::Deserialize;
pub use mxbot_common::config::{EncryptionStrategy, MatrixConfig};

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

#[derive(Deserialize, Default)]
pub struct SecurityConfig {
    /// Matrix user IDs allowed to invite the bot.
    #[serde(default)]
    pub allowed_inviters: Vec<String>,
    /// Matrix user IDs that can run admin commands (!adduser, !removeuser, …).
    #[serde(default)]
    pub admin_users: Vec<String>,
    #[serde(default)]
    pub encryption_strategy: EncryptionStrategy,
}

#[derive(Deserialize)]
pub struct ScheduleConfig {
    /// Matrix room ID where reminders and replies are posted.
    pub room_id: String,
    /// Cleaning interval in weeks (default 1 = every week).
    #[serde(default = "default_interval_weeks")]
    pub interval_weeks: u32,
    /// Weekday to send the initial reminder (0 = Mon … 6 = Sun).
    #[serde(default = "default_reminder_weekday")]
    pub reminder_weekday: u8,
    /// Weekday to send the final "not done yet" reminder.
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
    /// How many due weeks ahead to pre-materialize assignments.  Default: 26 (≈6 months).
    #[serde(default = "default_materialize_weeks")]
    pub materialize_weeks: u32,
}

/// Optional HTTP server that serves per-person iCal feeds.
/// If absent the `!ical` command falls back to Matrix file upload.
#[derive(Deserialize, Clone)]
pub struct ICalServerConfig {
    /// Address to bind the HTTP server to, e.g. "0.0.0.0:8080".
    pub bind_addr:  String,
    /// Public base URL shown to users, e.g. "https://cal.example.org".
    /// Must NOT end with a trailing slash.
    pub public_url: String,
}

fn default_interval_weeks() -> u32 { 1 }
fn default_materialize_weeks() -> u32 { 26 }
fn default_reminder_weekday() -> u8 { 0 }       // Monday
fn default_final_reminder_weekday() -> u8 { 6 } // Sunday
fn default_reminder_time() -> String { "09:00".to_owned() }
fn default_timezone() -> String { "UTC".to_owned() }
