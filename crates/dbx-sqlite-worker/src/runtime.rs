use rusqlite::{Connection, DatabaseName, OpenFlags};
use serde_json::json;
use sqlparser::ast::Statement;
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use crate::protocol::{WorkerBody, WorkerOp, WorkerRequest, WorkerResponse};

pub fn run_stdio() -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut connection = None;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let request: WorkerRequest = serde_json::from_str(&line).map_err(|e| format!("invalid worker request: {e}"))?;
        let response = handle(&mut connection, request);
        serde_json::to_writer(&mut stdout, &response).map_err(|e| e.to_string())?;
        stdout.write_all(b"\n").map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
        if matches!(response.body, WorkerBody::Ok { .. }) {
            // keep serving
        }
    }
    Ok(())
}

fn handle(connection: &mut Option<Connection>, request: WorkerRequest) -> WorkerResponse {
    let body = match request.op {
        WorkerOp::Open { path } => match open_database(&path) {
            Ok(opened) => {
                *connection = Some(opened);
                WorkerBody::ok()
            }
            Err(error) => WorkerBody::err(error),
        },
        WorkerOp::Query { sql, max_rows } => match connection.as_mut() {
            Some(conn) => query(conn, &sql, max_rows.unwrap_or(10_000)),
            None => WorkerBody::err("SQLite worker has no open database"),
        },
        WorkerOp::Backup { dest } => match connection.as_mut() {
            Some(conn) => backup(conn, &dest),
            None => WorkerBody::err("SQLite worker has no open database"),
        },
        WorkerOp::Restore { src } => match connection.as_mut() {
            Some(conn) => restore(conn, &src),
            None => WorkerBody::err("SQLite worker has no open database"),
        },
        WorkerOp::Ping => WorkerBody::pong(),
        WorkerOp::Close => {
            *connection = None;
            WorkerBody::ok()
        }
    };
    WorkerResponse { id: request.id, body }
}

fn open_database(path: &str) -> Result<Connection, String> {
    if path.trim().is_empty() {
        return Err("SQLite path is empty".to_string());
    }
    if path.contains('\0') {
        return Err("SQLite path contains NUL".to_string());
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI;
    if !Path::new(path).is_file() {
        return Err(format!("File does not exist: {path}"));
    }
    let conn = Connection::open_with_flags(path, flags).map_err(|e| format!("failed to open SQLite file: {e}"))?;
    conn.busy_timeout(Duration::from_secs(10)).map_err(|e| e.to_string())?;
    Ok(conn)
}

fn query(conn: &Connection, sql: &str, max_rows: usize) -> WorkerBody {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return WorkerBody::err("SQL is empty");
    }
    if sqlite_statement_returns_rows(trimmed) {
        query_statement(conn, trimmed, max_rows)
    } else {
        match conn.execute_batch(trimmed) {
            Ok(()) => WorkerBody::query(Vec::new(), Vec::new(), conn.changes(), false),
            Err(error) => WorkerBody::err(error.to_string()),
        }
    }
}

fn sqlite_statement_returns_rows(sql: &str) -> bool {
    if sqlite_starts_with_keyword(sql, &["SELECT", "PRAGMA", "EXPLAIN", "WITH"]) {
        return true;
    }
    let Ok(statements) = Parser::parse_sql(&SQLiteDialect {}, sql) else {
        return false;
    };
    let [statement] = statements.as_slice() else {
        return false;
    };
    match statement {
        Statement::Insert(insert) => insert.returning.is_some(),
        Statement::Update(update) => update.returning.is_some(),
        Statement::Delete(delete) => delete.returning.is_some(),
        _ => false,
    }
}

fn sqlite_starts_with_keyword(sql: &str, keywords: &[&str]) -> bool {
    let rest = skip_sqlite_trivia(sql);
    keywords.iter().any(|keyword| {
        rest.len() >= keyword.len()
            && rest[..keyword.len()].eq_ignore_ascii_case(keyword)
            && rest.as_bytes().get(keyword.len()).is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
    })
}

fn skip_sqlite_trivia(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if rest.starts_with("--") {
            rest = rest.split_once('\n').map(|(_, tail)| tail).unwrap_or("").trim_start();
            continue;
        }
        if let Some(rest_of_block) = rest.strip_prefix("/*") {
            rest = rest_of_block.split_once("*/").map(|(_, tail)| tail).unwrap_or("").trim_start();
            continue;
        }
        break;
    }
    rest
}

fn query_statement(conn: &Connection, sql: &str, max_rows: usize) -> WorkerBody {
    match conn.prepare(sql) {
        Ok(mut stmt) => {
            let column_count = stmt.column_count();
            if column_count == 0 {
                return match stmt.execute([]) {
                    Ok(changed) => WorkerBody::query(Vec::new(), Vec::new(), changed as u64, false),
                    Err(error) => WorkerBody::err(error.to_string()),
                };
            }
            let columns = stmt.column_names().iter().map(|name| (*name).to_string()).collect::<Vec<_>>();
            let mut rows = Vec::new();
            let mut truncated = false;
            match stmt.query([]) {
                Ok(mut mapped) => {
                    while let Ok(Some(row)) = mapped.next() {
                        if rows.len() >= max_rows {
                            truncated = true;
                            break;
                        }
                        let mut values = Vec::with_capacity(columns.len());
                        for index in 0..columns.len() {
                            match row.get_ref(index) {
                                Ok(value) => values.push(value_to_json(value)),
                                Err(error) => return WorkerBody::err(error.to_string()),
                            }
                        }
                        rows.push(values);
                    }
                }
                Err(error) => return WorkerBody::err(error.to_string()),
            }
            WorkerBody::query(columns, rows, 0, truncated)
        }
        Err(error) => WorkerBody::err(error.to_string()),
    }
}

fn backup(conn: &Connection, dest: &str) -> WorkerBody {
    if dest.trim().is_empty() || dest.contains('\0') {
        return WorkerBody::err("Backup destination is invalid");
    }
    if let Some(parent) = Path::new(dest).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return WorkerBody::err(error.to_string());
            }
        }
    }
    match conn.backup(DatabaseName::Main, dest, None) {
        Ok(()) => WorkerBody::ok(),
        Err(error) => WorkerBody::err(format!("SQLite backup failed: {error}")),
    }
}

fn restore(conn: &mut Connection, src: &str) -> WorkerBody {
    if src.trim().is_empty() || src.contains('\0') {
        return WorkerBody::err("Restore source is invalid");
    }
    match conn.restore(DatabaseName::Main, src, None::<fn(_)>) {
        Ok(()) => WorkerBody::ok(),
        Err(error) => WorkerBody::err(format!("SQLite restore failed: {error}")),
    }
}

fn value_to_json(value: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    match value {
        rusqlite::types::ValueRef::Null => json!(null),
        rusqlite::types::ValueRef::Integer(value) => json!(value),
        rusqlite::types::ValueRef::Real(value) => json!(value),
        rusqlite::types::ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
        rusqlite::types::ValueRef::Blob(value) => json!({ "$blob": value }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WorkerRequest;

    #[test]
    fn open_query_and_backup() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        rusqlite::Connection::open(&db).unwrap();
        let backup = dir.join("app.bak");
        let mut connection = None;
        let open =
            handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        assert!(matches!(open.body, WorkerBody::Ok { .. }));
        let create = handle(
            &mut connection,
            WorkerRequest { id: 2, op: WorkerOp::Query { sql: "CREATE TABLE t(id INTEGER)".into(), max_rows: None } },
        );
        assert!(matches!(create.body, WorkerBody::Ok { .. }));
        let insert = handle(
            &mut connection,
            WorkerRequest { id: 3, op: WorkerOp::Query { sql: "INSERT INTO t VALUES (1)".into(), max_rows: None } },
        );
        assert!(matches!(insert.body, WorkerBody::Ok { .. }));
        let select = handle(
            &mut connection,
            WorkerRequest { id: 4, op: WorkerOp::Query { sql: "SELECT id FROM t".into(), max_rows: Some(10) } },
        );
        match select.body {
            WorkerBody::Ok { rows, .. } => assert_eq!(rows.unwrap()[0][0], json!(1)),
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let backup_resp = handle(
            &mut connection,
            WorkerRequest { id: 5, op: WorkerOp::Backup { dest: backup.to_string_lossy().into() } },
        );
        assert!(matches!(backup_resp.body, WorkerBody::Ok { .. }), "{backup_resp:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn open_rejects_missing_files_instead_of_creating_them() {
        let path = std::env::temp_dir().join(format!("dbx-sqlite-worker-missing-{}.db", uuid_like()));
        let mut connection = None;
        let open = handle(
            &mut connection,
            WorkerRequest { id: 1, op: WorkerOp::Open { path: path.to_string_lossy().into() } },
        );
        match open.body {
            WorkerBody::Err { error } => assert!(error.contains("File does not exist"), "{error}"),
            other => panic!("expected missing-file error, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn multi_statement_scripts_run_every_statement() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-multi-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        rusqlite::Connection::open(&db).unwrap();
        let mut connection = None;
        let open =
            handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        assert!(matches!(open.body, WorkerBody::Ok { .. }));
        let script = handle(
            &mut connection,
            WorkerRequest {
                id: 2,
                op: WorkerOp::Query {
                    sql: "CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);".into(),
                    max_rows: None,
                },
            },
        );
        assert!(matches!(script.body, WorkerBody::Ok { .. }), "{script:?}");
        let select = handle(
            &mut connection,
            WorkerRequest { id: 3, op: WorkerOp::Query { sql: "SELECT id FROM t ORDER BY id".into(), max_rows: Some(10) } },
        );
        match select.body {
            WorkerBody::Ok { rows, .. } => {
                let rows = rows.expect("rows");
                assert_eq!(rows, vec![vec![json!(1)], vec![json!(2)]]);
            }
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn returning_dml_still_produces_a_result_set() {
        let dir = std::env::temp_dir().join(format!("dbx-sqlite-worker-returning-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.db");
        rusqlite::Connection::open(&db).unwrap();
        let mut connection = None;
        handle(&mut connection, WorkerRequest { id: 1, op: WorkerOp::Open { path: db.to_string_lossy().into() } });
        handle(
            &mut connection,
            WorkerRequest { id: 2, op: WorkerOp::Query { sql: "CREATE TABLE t(id INTEGER)".into(), max_rows: None } },
        );
        let insert = handle(
            &mut connection,
            WorkerRequest {
                id: 3,
                op: WorkerOp::Query { sql: "INSERT INTO t VALUES (7) RETURNING id".into(), max_rows: Some(10) },
            },
        );
        match insert.body {
            WorkerBody::Ok { rows, .. } => assert_eq!(rows.unwrap()[0][0], json!(7)),
            WorkerBody::Err { error } => panic!("{error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sqlite_statement_returns_rows_classifies_scripts() {
        assert!(sqlite_statement_returns_rows("SELECT 1"));
        assert!(sqlite_statement_returns_rows("-- comment\nWITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(sqlite_statement_returns_rows("INSERT INTO t VALUES (1) RETURNING id"));
        assert!(!sqlite_statement_returns_rows("CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (1);"));
        assert!(!sqlite_statement_returns_rows("INSERT INTO t VALUES (1)"));
    }

    fn uuid_like() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )
    }
}
