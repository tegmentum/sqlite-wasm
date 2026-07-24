//! Handle-keyed registry for the low-level db wrapper.

use std::collections::HashMap;

use crate::bindings::exports::sqlite::wasm::low_level::{ColumnType, ResultCode};
use crate::db::{self, Value};

pub struct StmtState {
    pub sql: String,
    pub db: u64,
    pub current_row: Option<Vec<Value>>,
    pub column_names: Vec<String>,
    pub bindings: Vec<Value>,
}

pub struct State {
    next: u64,
    pub dbs: HashMap<u64, db::Connection>,
    pub stmts: HashMap<u64, StmtState>,
}

impl State {
    pub fn new() -> Self {
        Self { next: 1, dbs: HashMap::new(), stmts: HashMap::new() }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        if self.next == 0 { self.next = 1; }
        id
    }

    pub fn add_db(&mut self, c: db::Connection) -> u64 {
        let id = self.alloc_id();
        self.dbs.insert(id, c);
        id
    }
    pub fn db(&self, h: u64) -> Option<&db::Connection> { self.dbs.get(&h) }
    pub fn remove_db(&mut self, h: u64) {
        self.dbs.remove(&h);
        self.stmts.retain(|_, s| s.db != h);
    }

    pub fn db_changes(&self, h: u64) -> u64 {
        self.dbs.get(&h).map(|c| c.changes() as u64).unwrap_or(0)
    }
    pub fn db_total_changes(&self, h: u64) -> u64 {
        self.dbs.get(&h).map(|c| c.total_changes() as u64).unwrap_or(0)
    }
    pub fn db_last_insert_rowid(&self, h: u64) -> i64 {
        self.dbs.get(&h).map(|c| c.last_insert_rowid()).unwrap_or(0)
    }

    pub fn prepare(&mut self, db: u64, sql: &str) -> Result<u64, ResultCode> {
        let _ = self.dbs.get(&db).ok_or(ResultCode::Misuse)?;
        let id = self.alloc_id();
        self.stmts.insert(id, StmtState {
            sql: sql.to_string(),
            db,
            current_row: None,
            column_names: Vec::new(),
            bindings: Vec::new(),
        });
        Ok(id)
    }

    pub fn step(&mut self, h: u64) -> ResultCode {
        let s = match self.stmts.get_mut(&h) {
            Some(s) => s,
            None => return ResultCode::Misuse,
        };
        let conn = match self.dbs.get(&s.db) {
            Some(c) => c,
            None => return ResultCode::Misuse,
        };
        let mut stmt = match conn.prepare(&s.sql) {
            Ok(st) => st,
            Err(_) => return ResultCode::Error,
        };
        s.column_names = stmt.column_names();
        let col_count = s.column_names.len();
        if stmt.bind_all(&s.bindings).is_err() {
            return ResultCode::Error;
        }
        match stmt.step() {
            Ok(db::StepResult::Row) => {
                let vals: Vec<Value> = (0..col_count).map(|i| stmt.column_value(i)).collect();
                s.current_row = Some(vals);
                ResultCode::Row
            }
            Ok(db::StepResult::Done) => {
                s.current_row = None;
                ResultCode::Done
            }
            Err(_) => ResultCode::Error,
        }
    }

    pub fn reset(&mut self, h: u64) -> ResultCode {
        if let Some(s) = self.stmts.get_mut(&h) { s.current_row = None; }
        ResultCode::Ok
    }
    pub fn finalize(&mut self, h: u64) -> ResultCode {
        self.stmts.remove(&h);
        ResultCode::Ok
    }

    pub fn bind(&mut self, h: u64, index: i32, value: Value) -> ResultCode {
        let s = match self.stmts.get_mut(&h) {
            Some(s) => s,
            None => return ResultCode::Misuse,
        };
        let idx = (index as usize).saturating_sub(1);
        if s.bindings.len() <= idx { s.bindings.resize(idx + 1, Value::Null); }
        s.bindings[idx] = value;
        ResultCode::Ok
    }

    pub fn column_count(&self, h: u64) -> i32 {
        self.stmts.get(&h).map(|s| s.column_names.len() as i32).unwrap_or(0)
    }
    pub fn column_name(&self, h: u64, idx: i32) -> String {
        self.stmts.get(&h)
            .and_then(|s| s.column_names.get(idx as usize).cloned())
            .unwrap_or_default()
    }
    pub fn column_type(&self, h: u64, idx: i32) -> ColumnType {
        match self.stmts.get(&h).and_then(|s| s.current_row.as_ref()?.get(idx as usize)) {
            Some(Value::Integer(_)) => ColumnType::Integer,
            Some(Value::Real(_)) => ColumnType::Float,
            Some(Value::Text(_)) => ColumnType::Text,
            Some(Value::Blob(_)) => ColumnType::Blob,
            _ => ColumnType::Null,
        }
    }
    pub fn column_int(&self, h: u64, idx: i32) -> i64 {
        match self.stmts.get(&h).and_then(|s| s.current_row.as_ref()?.get(idx as usize)) {
            Some(Value::Integer(i)) => *i,
            Some(Value::Real(r)) => *r as i64,
            _ => 0,
        }
    }
    pub fn column_double(&self, h: u64, idx: i32) -> f64 {
        match self.stmts.get(&h).and_then(|s| s.current_row.as_ref()?.get(idx as usize)) {
            Some(Value::Real(r)) => *r,
            Some(Value::Integer(i)) => *i as f64,
            _ => 0.0,
        }
    }
    pub fn column_text(&self, h: u64, idx: i32) -> String {
        match self.stmts.get(&h).and_then(|s| s.current_row.as_ref()?.get(idx as usize)) {
            Some(Value::Text(s)) => s.clone(),
            Some(Value::Integer(i)) => i.to_string(),
            Some(Value::Real(r)) => r.to_string(),
            _ => String::new(),
        }
    }
    pub fn column_blob(&self, h: u64, idx: i32) -> Vec<u8> {
        match self.stmts.get(&h).and_then(|s| s.current_row.as_ref()?.get(idx as usize)) {
            Some(Value::Blob(b)) => b.clone(),
            Some(Value::Text(s)) => s.as_bytes().to_vec(),
            _ => Vec::new(),
        }
    }
}
