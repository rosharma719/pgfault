use anyhow::{bail, Context, Result};
use pgfault_events::Event;
use pgfault_scenario::Scenario;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    sync::{Arc, Mutex},
};
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Semantic {
        #[serde(flatten)]
        coordinate: Event,
    },
    Fault {
        coordinate: Event,
        scenario: Box<Scenario>,
    },
    Lifecycle {
        connection_id: u64,
        event: String,
        detail: String,
    },
}
#[derive(Clone)]
pub struct Recorder(Arc<Mutex<File>>);
impl Recorder {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self(Arc::new(Mutex::new(
            OpenOptions::new().create_new(true).write(true).open(path)?,
        ))))
    }
    pub fn record(&self, record: Record) -> Result<()> {
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("trace lock poisoned"))?
            .write_all(&line)?;
        Ok(())
    }
    pub fn event(&self, e: Event) -> Result<()> {
        self.record(Record::Semantic { coordinate: e })
    }
    pub fn lifecycle(&self, id: u64, event: &str, detail: &str) -> Result<()> {
        self.record(Record::Lifecycle {
            connection_id: id,
            event: event.into(),
            detail: detail.into(),
        })
    }
}
/// Replays semantic coordinates, never process-global connection numbers or wall time.
pub fn replay(path: impl AsRef<Path>) -> Result<Vec<Scenario>> {
    let mut schedules = Vec::new();
    let mut unique = std::collections::HashSet::new();
    for (i, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let record: Record =
            serde_json::from_str(&line?).with_context(|| format!("trace line {}", i + 1))?;
        if let Record::Fault {
            coordinate: e,
            mut scenario,
        } = record
        {
            scenario.selector.application_name = e.startup.application_name;
            scenario.selector.user = e.startup.user;
            scenario.selector.database = e.startup.database;
            scenario.selector.transaction = Some(e.transaction_epoch);
            scenario.selector.query_cycle = Some(e.query_cycle);
            scenario.selector.statement = Some(e.statement_index);
            scenario.selector.sql_fingerprint = Some(e.sql_fingerprint);
            scenario.when.event = e.event;
            scenario.when.occurrence = Some(1);
            scenario.when.row = e.row_index;
            scenario.validate()?;
            if unique.insert(serde_json::to_string(&scenario)?) {
                schedules.push(*scenario);
            }
        }
    }
    if schedules.is_empty() {
        bail!("trace contains no fired faults");
    }
    Ok(schedules)
}
