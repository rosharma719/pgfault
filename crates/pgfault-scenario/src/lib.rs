use anyhow::{bail, Context, Result};
use pgfault_events::{Event, StatementClass, EVENTS};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub version: u32,
    pub name: String,
    #[serde(default, rename = "match")]
    pub selector: Selector,
    pub when: When,
    pub action: Action,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    pub application_name: Option<String>,
    pub user: Option<String>,
    pub database: Option<String>,
    /// The Nth connection accepted by this proxy process (1-based), for
    /// telling apart multiple connections opened by one client tool that
    /// don't otherwise differ in application_name/user/database.
    pub connection_ordinal: Option<u64>,
    pub transaction: Option<u64>,
    pub query_cycle: Option<u64>,
    pub statement: Option<u64>,
    pub sql_fingerprint: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    pub event: String,
    pub occurrence: Option<u64>,
    #[serde(default)]
    pub statement_class: Vec<StatementClass>,
    pub row: Option<u64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub suppress: Option<Suppress>,
    pub disconnect: Option<Disconnect>,
    pub delay: Option<Delay>,
    pub truncate_result: Option<Truncate>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suppress {
    pub current: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disconnect {
    pub side: Side,
    pub mode: Mode,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Frontend,
    Upstream,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Close,
    Reset,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delay {
    pub direction: Direction,
    pub duration: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Upstream,
    Downstream,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Truncate {
    pub after_rows: u64,
}
impl Scenario {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let s: Self = serde_yaml::from_str(
            &std::fs::read_to_string(path.as_ref())
                .with_context(|| format!("reading {}", path.as_ref().display()))?,
        )?;
        s.validate()?;
        Ok(s)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported scenario version {}", self.version);
        }
        if self.name.trim().is_empty() {
            bail!("scenario name is empty");
        }
        if !EVENTS.contains(&self.when.event.as_str()) {
            bail!("unknown semantic event: {}", self.when.event);
        }
        if self.when.occurrence == Some(0) || self.when.row == Some(0) {
            bail!("occurrence and row are one-based");
        }
        if self.selector.connection_ordinal == Some(0) {
            bail!("connection_ordinal is one-based");
        }
        if let Some(d) = &self.action.delay {
            let duration =
                humantime::parse_duration(&d.duration).context("invalid delay duration")?;
            if duration > Duration::from_secs(86400) {
                bail!("delay exceeds 24 hours");
            }
            let upstream = matches!(
                self.when.event.as_str(),
                "query.received"
                    | "query.forwarded"
                    | "statement.execution_started"
                    | "transaction.begin.requested"
                    | "transaction.commit.requested"
                    | "transaction.rollback.requested"
                    | "frontend.execute"
                    | "frontend.sync"
            );
            if upstream != (d.direction == Direction::Upstream) {
                bail!("delay direction must match the event direction");
            }
        }
        if self.action.suppress.is_none()
            && self.action.disconnect.is_none()
            && self.action.delay.is_none()
            && self.action.truncate_result.is_none()
        {
            bail!("scenario has no action");
        }
        if self.action.truncate_result.is_some() && self.when.event != "result.started" {
            bail!("truncate_result must target result.started");
        }
        Ok(())
    }
    pub fn selects(&self, e: &Event) -> bool {
        let m = &self.selector;
        m.application_name
            .as_ref()
            .is_none_or(|s| e.startup.application_name.as_ref() == Some(s))
            && m.user
                .as_ref()
                .is_none_or(|s| e.startup.user.as_ref() == Some(s))
            && m.database
                .as_ref()
                .is_none_or(|s| e.startup.database.as_ref() == Some(s))
            && m.connection_ordinal.is_none_or(|n| e.connection_id == n)
            && m.transaction.is_none_or(|n| e.transaction_epoch == n)
            && m.query_cycle.is_none_or(|n| e.query_cycle == n)
            && m.statement.is_none_or(|n| e.statement_index == n)
            && m.sql_fingerprint
                .as_ref()
                .is_none_or(|s| &e.sql_fingerprint == s)
            && (self.when.statement_class.is_empty()
                || self.when.statement_class.contains(&e.statement_class))
            && self.when.row.is_none_or(|n| e.row_index == Some(n))
    }
}
/// Each connection owns its matcher. Occurrences count only events satisfying all filters.
pub struct Matcher {
    pub scenario: Scenario,
    seen: u64,
    fired: bool,
}
impl Matcher {
    pub fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            seen: 0,
            fired: false,
        }
    }
    pub fn observe(&mut self, e: &Event) -> bool {
        if !e.semantic_reliable
            || self.fired
            || e.event != self.scenario.when.event
            || !self.scenario.selects(e)
        {
            return false;
        }
        self.seen += 1;
        if self.seen == self.scenario.when.occurrence.unwrap_or(1) {
            self.fired = true;
            true
        } else {
            false
        }
    }
    pub fn needs_gate(&self, boundary: &str) -> bool {
        !self.fired && self.scenario.when.event == boundary
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use pgfault_events::StartupMetadata;
    #[test]
    fn strict_validation() {
        assert!(serde_yaml::from_str::<Scenario>("version: 1\nname: x\nwhen: { event: result.row, typo: 3 }\naction: {suppress: {current: true}}\n").is_err());
        let s:Scenario=serde_yaml::from_str("version: 1\nname: x\nwhen: {event: result.row, occurrence: 0}\naction: {suppress: {current: true}}\n").unwrap();
        assert!(s.validate().is_err());
        let s: Scenario = serde_yaml::from_str("version: 1\nname: x\nmatch: {connection_ordinal: 0}\nwhen: {event: result.row}\naction: {suppress: {current: true}}\n").unwrap();
        assert!(s.validate().is_err());
    }
    fn event(connection_id: u64) -> Event {
        Event {
            connection_id,
            semantic_reliable: true,
            startup: StartupMetadata::default(),
            transaction_epoch: 1,
            query_cycle: 1,
            statement_index: 1,
            statement_class: StatementClass::Other,
            sql_fingerprint: String::new(),
            event: "result.row".into(),
            occurrence: 1,
            row_index: None,
        }
    }
    #[test]
    fn connection_ordinal_distinguishes_same_named_connections() {
        let s: Scenario = serde_yaml::from_str(
            "version: 1\nname: x\nmatch: {connection_ordinal: 2}\nwhen: {event: result.row}\naction: {suppress: {current: true}}\n",
        )
        .unwrap();
        assert!(!s.selects(&event(1)));
        assert!(s.selects(&event(2)));
        assert!(!s.selects(&event(3)));
    }
}
