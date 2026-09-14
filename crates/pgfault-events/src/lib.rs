use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StatementClass {
    Begin,
    Commit,
    Rollback,
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    Ddl,
    #[default]
    Other,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartupMetadata {
    pub user: Option<String>,
    pub database: Option<String>,
    pub application_name: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Statement {
    pub class: StatementClass,
    pub fingerprint: String,
}
impl Statement {
    pub fn new(sql: &[u8]) -> Self {
        Self {
            class: classify(sql),
            fingerprint: format!("{:x}", Sha256::digest(sql)),
        }
    }
}
/// Conservative lexical scan: quoted strings/identifiers, dollar quoting and nested comments
/// are skipped. Multiple statements are deliberately classified Other.
pub fn classify(sql: &[u8]) -> StatementClass {
    let mut i = 0;
    let mut words: Vec<String> = Vec::new();
    let mut ended = false;
    while i < sql.len() {
        let b = sql[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if sql[i..].starts_with(b"--") {
            while i < sql.len() && sql[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if sql[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < sql.len() && depth > 0 {
                if sql[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if sql[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return StatementClass::Other;
            }
            continue;
        }
        if ended {
            return StatementClass::Other;
        }
        if b == b';' {
            ended = true;
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            let quote = b;
            i += 1;
            let mut closed = false;
            while i < sql.len() {
                if sql[i] == b'\\' {
                    i = (i + 2).min(sql.len());
                    continue;
                }
                if sql[i] == quote {
                    i += 1;
                    if i < sql.len() && sql[i] == quote {
                        i += 1;
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return StatementClass::Other;
            }
            continue;
        }
        if b == b'$' {
            let mut j = i + 1;
            while j < sql.len() && (sql[j].is_ascii_alphanumeric() || sql[j] == b'_') {
                j += 1;
            }
            if j < sql.len() && sql[j] == b'$' {
                let delim = &sql[i..=j];
                let start = j + 1;
                if let Some(n) = sql[start..].windows(delim.len()).position(|w| w == delim) {
                    i = start + n + delim.len();
                    continue;
                } else {
                    return StatementClass::Other;
                }
            }
        }
        if b.is_ascii_alphabetic() {
            let start = i;
            while i < sql.len() && (sql[i].is_ascii_alphanumeric() || sql[i] == b'_') {
                i += 1;
            }
            words.push(String::from_utf8_lossy(&sql[start..i]).to_ascii_uppercase());
        } else {
            i += 1;
        }
    }
    match words.first().map(String::as_str) {
        Some("BEGIN" | "START") => StatementClass::Begin,
        Some("COMMIT" | "END") if !words.iter().any(|w| w == "PREPARED" || w == "CHAIN") => {
            StatementClass::Commit
        }
        Some("ROLLBACK" | "ABORT")
            if !words
                .iter()
                .any(|w| w == "TO" || w == "PREPARED" || w == "CHAIN") =>
        {
            StatementClass::Rollback
        }
        Some("SELECT" | "VALUES" | "TABLE") => StatementClass::Select,
        Some("INSERT") => StatementClass::Insert,
        Some("UPDATE") => StatementClass::Update,
        Some("DELETE") => StatementClass::Delete,
        Some("MERGE") => StatementClass::Merge,
        Some("CREATE" | "ALTER" | "DROP" | "TRUNCATE") => StatementClass::Ddl,
        _ => StatementClass::Other,
    }
}
pub const EVENTS: &[&str] = &[
    "query.received",
    "query.forwarded",
    "statement.execution_started",
    "statement.execution_completed",
    "transaction.begin.requested",
    "transaction.commit.requested",
    "transaction.commit.completed",
    "transaction.rollback.requested",
    "transaction.rollback.completed",
    "transaction.implicit.completed",
    "transaction.failed",
    "result.started",
    "result.row",
    "result.completed",
    "connection.ready",
    "frontend.execute",
    "frontend.sync",
    "backend.error",
    "backend.command_complete",
    "backend.ready",
];
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub connection_id: u64,
    pub semantic_reliable: bool,
    #[serde(flatten)]
    pub startup: StartupMetadata,
    pub transaction_epoch: u64,
    pub query_cycle: u64,
    pub statement_index: u64,
    pub statement_class: StatementClass,
    pub sql_fingerprint: String,
    pub event: String,
    pub occurrence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_index: Option<u64>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conservative_sql() {
        assert_eq!(
            classify(b" /* outer /* nested */ x */ -- hi\nCOMMIT;"),
            StatementClass::Commit
        );
        for sql in [
            "BEGIN; INSERT INTO x VALUES(1); COMMIT",
            "COMMIT PREPARED 'x'",
            "ROLLBACK TO SAVEPOINT x",
            "COMMIT AND CHAIN",
            "/* unterminated",
        ] {
            assert_eq!(classify(sql.as_bytes()), StatementClass::Other);
        }
        assert_eq!(classify(b"SELECT ';', $$a;b$$;"), StatementClass::Select);
    }
}
