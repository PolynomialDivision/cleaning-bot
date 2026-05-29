use serde::Deserialize;
pub use mxbot_common::config::{EncryptionStrategy, MatrixConfig};

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
    /// IANA timezone string used for weekday calculations (e.g. "Europe/Berlin").
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Weekday to post a weekly stats summary (0 = Mon … 6 = Sun).
    /// Omit or comment out to disable the automatic summary.
    pub summary_weekday: Option<u8>,
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
fn default_reminder_weekday() -> u8 { 0 }       // Monday
fn default_final_reminder_weekday() -> u8 { 6 } // Sunday
fn default_timezone() -> String { "UTC".to_owned() }
