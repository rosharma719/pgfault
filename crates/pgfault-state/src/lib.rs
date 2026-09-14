//! Protocol observation only. All forwarding uses the original frames.
use pgfault_events::{Event, StartupMetadata, Statement, StatementClass};
use pgfault_wire::{cstr, text, Frame};
use std::collections::{HashMap, VecDeque};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TxState {
    #[default]
    Idle,
    InTransaction,
    FailedTransaction,
}
#[derive(Clone)]
enum Mutation {
    Parse(Vec<u8>, Statement),
    Bind(Vec<u8>, Statement),
    Close(u8, Vec<u8>),
}
#[derive(Default)]
struct Cycle {
    id: u64,
    statements: VecDeque<Statement>,
    last: Statement,
    rows: u64,
    command: String,
    error: bool,
    extended: bool,
    sealed: bool,
    statement_index: u64,
    executions: u64,
    epoch_allocated: bool,
    mutations: VecDeque<Mutation>,
}
pub struct ConnectionState {
    pub id: u64,
    pub startup: StartupMetadata,
    pub tx_state: TxState,
    pub tx_epoch: u64,
    pub prepared: HashMap<Vec<u8>, Statement>,
    pub portals: HashMap<Vec<u8>, Statement>,
    cycles: VecDeque<Cycle>,
    next_cycle: u64,
    reliable: bool,
    confirmed_prepared: HashMap<Vec<u8>, Statement>,
    confirmed_portals: HashMap<Vec<u8>, Statement>,
    counters: HashMap<String, u64>,
}
impl ConnectionState {
    pub fn new(id: u64, startup: StartupMetadata) -> Self {
        Self {
            id,
            startup,
            tx_state: TxState::Idle,
            tx_epoch: 0,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            cycles: VecDeque::new(),
            next_cycle: 0,
            reliable: true,
            confirmed_prepared: HashMap::new(),
            confirmed_portals: HashMap::new(),
            counters: HashMap::new(),
        }
    }
    fn cycle(&mut self, extended: bool) -> &mut Cycle {
        if !extended || self.cycles.back().is_none_or(|c| c.sealed) {
            self.next_cycle += 1;
            self.cycles.push_back(Cycle {
                id: self.next_cycle,
                extended,
                sealed: !extended,
                ..Cycle::default()
            });
        }
        if self.cycles.len() > 1 {
            self.reliable = false;
        }
        self.cycles.back_mut().unwrap()
    }
    fn event(&mut self, name: &str, front: bool) -> Event {
        let c = if front {
            self.cycles.back()
        } else {
            self.cycles.front()
        };
        let s = c
            .map(|c| {
                if front {
                    c.statements.back().unwrap_or(&c.last)
                } else {
                    c.statements.front().unwrap_or(&c.last)
                }
            })
            .cloned()
            .unwrap_or_default();
        let count = self.counters.entry(name.into()).or_default();
        *count += 1;
        Event {
            connection_id: self.id,
            semantic_reliable: self.reliable,
            startup: self.startup.clone(),
            transaction_epoch: self.tx_epoch,
            query_cycle: c.map_or(0, |c| c.id),
            statement_index: c.map_or(0, |c| {
                if front {
                    c.executions
                } else if c.statements.is_empty() {
                    c.statement_index.max(1)
                } else {
                    c.statement_index + 1
                }
            }),
            statement_class: s.class,
            sql_fingerprint: s.fingerprint,
            event: name.into(),
            occurrence: *count,
            row_index: if name == "result.row" {
                c.map(|c| c.rows)
            } else {
                None
            },
        }
    }
    pub fn frontend(&mut self, f: &Frame) -> Vec<Event> {
        let mut names = Vec::new();
        match f.tag() {
            b'P' => {
                if let Some((name, rest)) = cstr(f.body()) {
                    if let Some((sql, _)) = cstr(rest) {
                        let statement = Statement::new(sql);
                        self.prepared.insert(name.to_vec(), statement.clone());
                        self.cycle(true)
                            .mutations
                            .push_back(Mutation::Parse(name.to_vec(), statement));
                    }
                }
                self.cycle(true);
            }
            b'B' => {
                if let Some((portal, rest)) = cstr(f.body()) {
                    if let Some((stmt, _)) = cstr(rest) {
                        if let Some(s) = self.prepared.get(stmt).cloned() {
                            self.portals.insert(portal.to_vec(), s.clone());
                            self.cycle(true)
                                .mutations
                                .push_back(Mutation::Bind(portal.to_vec(), s));
                        } else {
                            self.portals.remove(portal);
                        }
                    }
                }
                self.cycle(true);
            }
            b'C' => {
                if let Some((&kind, rest)) = f.body().split_first() {
                    if let Some((name, _)) = cstr(rest) {
                        self.cycle(true)
                            .mutations
                            .push_back(Mutation::Close(kind, name.to_vec()));
                        if kind == b'S' {
                            self.prepared.remove(name);
                        } else if kind == b'P' {
                            self.portals.remove(name);
                        }
                    }
                }
            }
            b'Q' | b'E' => {
                let s = if f.tag() == b'Q' {
                    cstr(f.body())
                        .map(|(sql, _)| Statement::new(sql))
                        .unwrap_or_default()
                } else {
                    cstr(f.body())
                        .and_then(|(name, _)| self.portals.get(name))
                        .cloned()
                        .unwrap_or_default()
                };
                let class = s.class;
                if f.tag() == b'Q' {
                    self.prepared.remove(b"".as_slice());
                    self.portals.remove(b"".as_slice());
                    self.confirmed_prepared.remove(b"".as_slice());
                    self.confirmed_portals.remove(b"".as_slice());
                }
                let c = self.cycle(f.tag() == b'E');
                c.statements.push_back(s);
                c.executions += 1;

                names.push(if f.tag() == b'Q' {
                    "query.received"
                } else {
                    "frontend.execute"
                });
                names.push("statement.execution_started");
                match class {
                    StatementClass::Begin => names.push("transaction.begin.requested"),
                    StatementClass::Commit => names.push("transaction.commit.requested"),
                    StatementClass::Rollback => names.push("transaction.rollback.requested"),
                    _ => {}
                }
            }
            b'S' => {
                self.cycle(true).sealed = true;
                names.push("frontend.sync");
            }
            _ => {}
        }
        // Allocate transaction epochs when work begins from idle; the backend remains authoritative.
        if self.tx_state == TxState::Idle
            && self.cycles.len() == 1
            && self.cycles.front().is_some_and(|c| !c.epoch_allocated)
            && matches!(f.tag(), b'Q' | b'E')
        {
            self.tx_epoch += 1;
            self.cycles.front_mut().unwrap().epoch_allocated = true;
        }
        names.into_iter().map(|n| self.event(n, true)).collect()
    }
    pub fn forwarded(&mut self) -> Event {
        self.event("query.forwarded", true)
    }
    /// A completion frame may need to be gated BEFORE it becomes client-visible.
    pub fn completion_boundary(&self, f: &Frame) -> Option<&'static str> {
        if f.tag() != b'C' || !self.reliable {
            return None;
        }
        let c = self.cycles.front()?;
        let class = c.statements.front().unwrap_or(&c.last).class;
        let tag = text(f.body())?;
        match (class, tag.as_str()) {
            (StatementClass::Commit, "COMMIT") => Some("transaction.commit.completed"),
            (StatementClass::Rollback, "ROLLBACK") => Some("transaction.rollback.completed"),
            (StatementClass::Commit, _) | (StatementClass::Rollback, _) => None,
            _ if self.tx_state == TxState::Idle
                && class != StatementClass::Begin
                && class != StatementClass::Other =>
            {
                Some("transaction.implicit.completed")
            }
            _ => None,
        }
    }
    pub fn backend(&mut self, f: &Frame) -> Vec<Event> {
        let mut out = Vec::new();
        match f.tag() {
            b'S' => {
                if let Some((b"application_name", rest)) = cstr(f.body()) {
                    if let Some((value, _)) = cstr(rest) {
                        self.startup.application_name =
                            Some(String::from_utf8_lossy(value).into_owned());
                    }
                }
            }
            b'1' | b'2' | b'3' => {
                let mutation = self
                    .cycles
                    .front_mut()
                    .and_then(|c| c.mutations.pop_front());
                match mutation {
                    Some(Mutation::Parse(name, s)) if f.tag() == b'1' => {
                        self.confirmed_prepared.insert(name, s);
                    }
                    Some(Mutation::Bind(name, s)) if f.tag() == b'2' => {
                        self.confirmed_portals.insert(name, s);
                    }
                    Some(Mutation::Close(kind, name)) if f.tag() == b'3' => {
                        if kind == b'S' {
                            self.confirmed_prepared.remove(&name);
                        } else {
                            self.confirmed_portals.remove(&name);
                        }
                    }
                    _ => {
                        self.reliable = false;
                    }
                }
            }
            b'T' => out.push(self.event("result.started", false)),
            b'D' => {
                if let Some(c) = self.cycles.front_mut() {
                    c.rows += 1;
                }
                out.push(self.event("result.row", false));
            }
            b'C' => {
                out.push(self.event("backend.command_complete", false));
                out.push(self.event("statement.execution_completed", false));
                out.push(self.event("result.completed", false));
                if let Some(c) = self.cycles.front_mut() {
                    c.command = text(f.body()).unwrap_or_default();
                    if let Some(s) = c.statements.pop_front() {
                        c.last = s;
                    }
                    if c.extended {
                        c.statement_index += 1;
                    }
                    c.rows = 0;
                }
            }
            b's' => {
                if let Some(c) = self.cycles.front_mut() {
                    if let Some(s) = c.statements.pop_front() {
                        c.last = s;
                    }
                    c.rows = 0;
                }
            }
            b'E' => {
                if let Some(c) = self.cycles.front_mut() {
                    c.error = true;
                    c.mutations.clear();
                }
                self.prepared = self.confirmed_prepared.clone();
                self.portals = self.confirmed_portals.clone();
                out.push(self.event("backend.error", false));
            }
            b'Z' => {
                let new = match f.body() {
                    [b'I'] => TxState::Idle,
                    [b'T'] => TxState::InTransaction,
                    [b'E'] => TxState::FailedTransaction,
                    _ => return out,
                };
                out.push(self.event("backend.ready", false));
                if new == TxState::FailedTransaction && self.tx_state != new {
                    out.push(self.event("transaction.failed", false));
                }
                let completed = self.cycles.front().and_then(|c| {
                    if new != TxState::Idle || c.error || !self.reliable {
                        return None;
                    }
                    match (c.last.class, c.command.as_str()) {
                        (StatementClass::Commit, "COMMIT") => Some("transaction.commit.completed"),
                        (StatementClass::Rollback, "ROLLBACK") => {
                            Some("transaction.rollback.completed")
                        }
                        (StatementClass::Commit, _) | (StatementClass::Rollback, _) => None,
                        _ if self.tx_state == TxState::Idle
                            && !c.command.is_empty()
                            && c.last.class != StatementClass::Other =>
                        {
                            Some("transaction.implicit.completed")
                        }
                        _ => None,
                    }
                });
                if let Some(n) = completed {
                    out.push(self.event(n, false));
                }
                self.tx_state = new;
                out.push(self.event("connection.ready", false));
                self.cycles.pop_front();
                if new == TxState::Idle {
                    self.portals.clear();
                    self.confirmed_portals.clear();
                }
                if self.cycles.is_empty() {
                    self.reliable = true;
                    self.prepared = self.confirmed_prepared.clone();
                    self.portals = self.confirmed_portals.clone();
                }
            }
            _ => {}
        }
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use pgfault_wire::frame;
    fn state() -> ConnectionState {
        ConnectionState::new(1, StartupMetadata::default())
    }
    #[test]
    fn commit_requires_idle_and_success() {
        let mut s = state();
        s.frontend(&frame(b'Q', b"BEGIN\0"));
        s.backend(&frame(b'C', b"BEGIN\0"));
        s.backend(&frame(b'Z', b"T"));
        s.frontend(&frame(b'Q', b"COMMIT\0"));
        assert_eq!(
            s.completion_boundary(&frame(b'C', b"COMMIT\0")),
            Some("transaction.commit.completed")
        );
        assert!(!s
            .backend(&frame(b'C', b"COMMIT\0"))
            .iter()
            .any(|e| e.event == "transaction.commit.completed"));
        assert!(s
            .backend(&frame(b'Z', b"I"))
            .iter()
            .any(|e| e.event == "transaction.commit.completed" && e.transaction_epoch == 1));
        s.frontend(&frame(b'Q', b"COMMIT\0"));
        s.backend(&frame(b'C', b"ROLLBACK\0"));
        assert!(!s
            .backend(&frame(b'Z', b"I"))
            .iter()
            .any(|e| e.event == "transaction.commit.completed"));
    }
    #[test]
    fn unnamed_slots_and_failed_transaction() {
        let mut s = state();
        s.frontend(&frame(b'P', b"\0INSERT INTO x VALUES($1)\0\0\0"));
        s.frontend(&frame(b'B', b"\0\0\0\0\0\0\0\0"));
        let e = s.frontend(&frame(b'E', b"\0\0\0\0\0"));
        assert_eq!(e[0].statement_class, StatementClass::Insert);
        s.frontend(&frame(b'S', b""));
        s.backend(&frame(b'E', b"\0"));
        assert!(s
            .backend(&frame(b'Z', b"E"))
            .iter()
            .any(|e| e.event == "transaction.failed"));
    }
}
