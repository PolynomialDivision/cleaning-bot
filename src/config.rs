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
    /// If set, the house's directory says who is who (src/directory.rs): one resident's
    /// Matrix accounts are one person, and the plan follows who lives here.
    pub directory: Option<DirectorySection>,
    /// If set (with `[directory]`), every resident with a house account gets their turns in a
    /// Nextcloud calendar of their own (src/calendars.rs).
    pub nextcloud: Option<NextcloudConfig>,
    /// The residents' website (src/web.rs), off unless enabled.
    #[serde(default)]
    pub web: WebConfig,
}

/// `[directory]`: mxbot-common's reader of the house's directory, plus what this bot follows
/// in it. Its own keys are taken out before the rest goes to the reader, which refuses keys
/// it doesn't know.
#[derive(Clone, Debug)]
pub struct DirectorySection {
    pub reader: DirectoryConfig,
    /// Who lives here (`residents_group`: one group name or a list of them, default "": off):
    /// a person in the plan who is in none of them leaves every cleaning group
    /// (src/directory/reconcile.rs). Living here is being in ANY of them: full members
    /// (`residents`) and people who live here for a while (`subtenants`) alike. Empty ("" or
    /// []): nobody leaves for not living here; switched on by naming the groups, once the first
    /// reads have linked everyone (the first departures are held for an admin anyway).
    /// Trimmed, sorted, each once.
    pub residents_groups: Vec<String>,
}

impl DirectorySection {
    /// Whether `person` lives here by these rules: in one of the groups. True for everyone
    /// when no group is named (nobody leaves for it, every kitchen member joins).
    pub fn lives_here(groups: &[String], person: &mxbot_common::directory::Person) -> bool {
        groups.is_empty() || groups.iter().any(|g| person.groups.contains(g))
    }
}

/// `residents_group` as it is written: one name, or a list of them.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// The groups of `residents_group`: trimmed, empty names dropped, sorted, each once. A name with
/// a comma is refused: the groups are remembered as one comma-separated key
/// (`DirectoryMemory::reviewed`).
fn residents_groups(value: toml::Value) -> Result<Vec<String>, String> {
    const WHAT: &str = "[directory] residents_group: a group name or a list of group names";
    let names = match OneOrMany::deserialize(value).map_err(|_| WHAT.to_owned())? {
        OneOrMany::One(name) => vec![name],
        OneOrMany::Many(names) => names,
    };
    let mut out: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    if let Some(bad) = out.iter().find(|n| n.contains(',')) {
        return Err(format!(
            "[directory] residents_group: {bad:?} has a comma (one name per entry)"
        ));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

impl<'de> Deserialize<'de> for DirectorySection {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut table = toml::Table::deserialize(d)?;
        let residents_groups = match table.remove("residents_group") {
            None => Vec::new(),
            Some(value) => residents_groups(value).map_err(D::Error::custom)?,
        };
        let reader =
            DirectoryConfig::deserialize(toml::Value::Table(table)).map_err(D::Error::custom)?;
        Ok(DirectorySection {
            reader,
            residents_groups,
        })
    }
}

/// The website (src/web.rs): the plan in a browser, read only, off unless enabled. It sits
/// behind the house's login at the reverse proxy, which passes the person on as X-House-*
/// headers and proves itself with the secret header X-Cleaning-Proxy-Secret
/// ([`crate::web::PROXY_SECRET_ENV`] in the environment). Its own listener, never the
/// iCal server's: that one is public.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_web_bind")]
    pub bind_addr: String,
    /// Its address as people open it, e.g. `https://putzplan.example.org`.
    #[serde(default)]
    pub public_url: String,
    /// Groups (X-House-Groups) that may see it; empty: nobody with a login.
    #[serde(default)]
    pub allowed_groups: Vec<String>,
    /// Requests through the proxy without a login see the plan as a guest (no "you", no
    /// own turns): for a proxy that asks for the login only from outside the house.
    #[serde(default)]
    pub guests: bool,
    /// The house's start page, linked in the house bar ("⌂ Name"). Unset: public_url's
    /// parent domain (`https://putzplan.example.org` → `https://example.org/`).
    #[serde(default)]
    pub home_url: Option<String>,
    /// Its name in the house bar. Unset: from its address (example.org → "Example").
    #[serde(default)]
    pub home_name: Option<String>,
}

fn default_web_bind() -> String {
    "0.0.0.0:8080".into()
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            enabled: false,
            bind_addr: default_web_bind(),
            public_url: String::new(),
            allowed_groups: Vec::new(),
            guests: false,
            home_url: None,
            home_name: None,
        }
    }
}

impl WebConfig {
    /// Checked at startup when enabled: the addresses and names.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if http_host(&self.public_url).is_none() {
            anyhow::bail!("[web] public_url must be the website's http(s) address");
        }
        if self.bind_addr.parse::<std::net::SocketAddr>().is_err() {
            anyhow::bail!("[web] bind_addr {:?} is not ip:port", self.bind_addr);
        }
        if let Some(home) = &self.home_url {
            if http_host(home).is_none() {
                anyhow::bail!("[web] home_url must be the start page's http(s) address");
            }
        }
        if self
            .home_name
            .as_ref()
            .is_some_and(|n| n.trim().is_empty() || n.chars().count() > 40)
        {
            anyhow::bail!("[web] home_name: 1–40 characters");
        }
        if self.allowed_groups.iter().any(|g| g.trim().is_empty()) {
            anyhow::bail!("[web] allowed_groups: no empty names");
        }
        Ok(())
    }
}

/// The host (and port) of an http(s) address; `None` for anything else.
fn http_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty() && !host.contains(char::is_whitespace)).then_some(host)
}

/// The bot's own Nextcloud account, which owns one calendar per resident and shares it with
/// them (read only).
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct NextcloudConfig {
    /// The account's calendar home, e.g.
    /// `https://cloud.example.org/remote.php/dav/calendars/cleaning-bot/`.
    pub account_url: String,
    /// The account (its Nextcloud user ID).
    pub username: String,
    /// The environment variable with the account's app password, never the password itself.
    #[serde(default = "default_password_env")]
    pub password_env: String,
    /// How often everything is compared with the calendars (they follow every plan change
    /// within a minute anyway).
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
    /// How many weeks ahead the calendars show (1–104).
    #[serde(default = "default_calendar_weeks")]
    pub weeks: u32,
}

impl NextcloudConfig {
    pub fn refresh(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.refresh_minutes.max(1) * 60)
    }

    /// The Nextcloud Calendar app's address on the account's server.
    pub fn web_url(&self) -> Option<String> {
        let url = reqwest::Url::parse(&self.account_url).ok()?;
        Some(format!(
            "{}/apps/calendar",
            url.origin().ascii_serialization()
        ))
    }
}

fn default_password_env() -> String {
    "CALDAV_PASSWORD".to_owned()
}
fn default_refresh_minutes() -> u64 {
    15
}
fn default_calendar_weeks() -> u32 {
    26
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
        assert!(!config.web.enabled);
        // With the commented-out section filled in, it reads too.
        let web = include_str!("../config.example.toml")
            .replace("# [web]", "[web]")
            .replace("# enabled", "enabled")
            .replace(
                "# bind_addr  = \"0.0.0.0:8081\"",
                "bind_addr  = \"0.0.0.0:8081\"",
            )
            .replace(
                "# public_url = \"https://putzplan",
                "public_url = \"https://putzplan",
            )
            .replace("# allowed_groups", "allowed_groups");
        let config: Config = toml::from_str(&web).unwrap();
        assert!(config.web.enabled && config.web.validate().is_ok());
        assert_eq!(config.web.allowed_groups, ["residents"]);
        let directory = include_str!("../config.example.toml")
            .replace("# [directory]", "[directory]")
            .replace("# provider", "provider")
            .replace("# api_url", "api_url")
            .replace("# server_name", "server_name");
        let section = toml::from_str::<Config>(&directory)
            .unwrap()
            .directory
            .unwrap();
        // Commented out in the example: off.
        assert!(section.residents_groups.is_empty());
        let with_residents = directory.replace("# residents_group =", "residents_group =");
        let residents = toml::from_str::<Config>(&with_residents)
            .unwrap()
            .directory
            .unwrap()
            .residents_groups;
        assert_eq!(residents, ["residents", "subtenants"]);
        let d = section.reader;
        assert_eq!(
            d.provider,
            mxbot_common::directory::DirectoryProvider::Authentik
        );
        assert_eq!(d.matrix_ids_attribute, "matrix_ids");
        assert_eq!(d.token_env, "DIRECTORY_TOKEN");
        assert_eq!(d.refresh_minutes, 15);
        assert!(toml::from_str::<Config>(&directory)
            .unwrap()
            .nextcloud
            .is_none());
        let nextcloud = include_str!("../config.example.toml")
            .replace("# [nextcloud]", "[nextcloud]")
            .replace("# account_url", "account_url")
            .replace("# username", "username");
        let n = toml::from_str::<Config>(&nextcloud)
            .unwrap()
            .nextcloud
            .unwrap();
        assert_eq!(n.username, "cleaning-bot");
        assert_eq!(n.password_env, "CALDAV_PASSWORD");
        assert_eq!(n.refresh_minutes, 15);
        assert_eq!(n.weeks, 26);
        assert_eq!(
            n.web_url().as_deref(),
            Some("https://cloud.example.org/apps/calendar")
        );
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
        assert!(config.directory.is_some() && config.web.enabled && config.web.guests);
        config.web.validate().unwrap();
        let n = config.nextcloud.as_ref().unwrap();
        assert_eq!(n.password_env, "CALDAV_PASSWORD");
        assert!(n.account_url.ends_with("/calendars/cleaning-bot/"));
        config.security.validate().unwrap();
    }

    #[test]
    fn the_directory_section_takes_the_residents_group_and_refuses_typos() {
        let section = |extra: &str| {
            toml::from_str::<DirectorySection>(&format!(
                "provider = \"authentik\"\napi_url = \"https://login.example.org/api/v3\"\n{extra}"
            ))
        };
        let groups = |extra: &str| section(extra).unwrap().residents_groups;
        // Off unless named: an upgrade starts no departures by itself.
        assert!(groups("").is_empty());
        // One name, as before the list (an older config)
        assert_eq!(groups("residents_group = \" bewohner \""), ["bewohner"]);
        // A list: living here is being in any of them (sorted, each once, trimmed)
        assert_eq!(
            groups("residents_group = [\"subtenants\", \" residents\", \"residents\", \"\"]"),
            ["residents", "subtenants"]
        );
        // Empty: nobody leaves because of the directory.
        assert!(groups("residents_group = \"\"").is_empty());
        assert!(groups("residents_group = []").is_empty());
        assert!(groups("residents_group = [\" \"]").is_empty());
        let s = section("residents_group = \"r\"\nrefresh_minutes = 5").unwrap();
        assert_eq!(s.reader.refresh_minutes, 5);
        for bad in [
            "residents_group = 3",
            "residents_group = [\"r\", 3]",
            "residents_group = { name = \"r\" }",
            "residents_group = \"residents,subtenants\"",
            "residents_grup = \"r\"",
        ] {
            assert!(section(bad).is_err(), "{bad}");
        }
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

    fn web(raw: &str) -> WebConfig {
        toml::from_str(raw).unwrap()
    }

    #[test]
    fn the_website_is_off_by_default_and_checked_when_on() {
        let off = WebConfig::default();
        assert!(!off.enabled);
        assert_eq!(off.bind_addr, "0.0.0.0:8080");
        assert!(off.validate().is_ok());
        assert!(toml::from_str::<WebConfig>("enabld = true").is_err());

        let ok = web("enabled = true\npublic_url = \"https://putzplan.example.org\"");
        assert!(ok.validate().is_ok());
        for bad in [
            "enabled = true",
            "enabled = true\npublic_url = \"putzplan.example.org\"",
            "enabled = true\npublic_url = \"ftp://putzplan.example.org\"",
            "enabled = true\npublic_url = \"https://\"",
            "enabled = true\npublic_url = \"https://p.example.org\"\nbind_addr = \"nope\"",
            "enabled = true\npublic_url = \"https://p.example.org\"\nhome_url = \"example.org\"",
            "enabled = true\npublic_url = \"https://p.example.org\"\nhome_name = \"\"",
            "enabled = true\npublic_url = \"https://p.example.org\"\nallowed_groups = [\"\"]",
        ] {
            assert!(web(bad).validate().is_err(), "{bad}");
        }
        let full = web(
            "enabled = true\nbind_addr = \"127.0.0.1:8090\"\npublic_url = \"https://p.example.org\"\n\
             allowed_groups = [\"residents\"]\nhome_url = \"https://example.org/\"\nhome_name = \"Example\"",
        );
        assert!(full.validate().is_ok());
        assert_eq!(full.allowed_groups, ["residents"]);
    }

    /// The calendar feeds are gone: a config from before, with `[ical_server]`,
    /// still loads (the section is ignored), so the bot starts before its
    /// config is renewed.
    #[test]
    fn an_old_config_with_the_feed_server_still_loads() {
        let old = format!(
            "{}\n[ical_server]\nbind_addr = \"0.0.0.0:80\"\npublic_url = \"https://cal.example.org\"\n",
            include_str!("../config.example.toml")
        );
        assert!(toml::from_str::<Config>(&old).is_ok());
    }
}
