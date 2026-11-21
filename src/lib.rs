//! Cinder — crash-safe embedded SQL database.
//!
//! WAL with CRC32, memtable, sorted runs (SSTables), secondary indexes,
//! and multi-statement transactions. Pure Rust. Zero dependencies.
//!
//! # Quick start
//!
//! ```rust
//! use cinder::Database;
//!
//! let mut db = Database::open("data").unwrap();
//! db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)").unwrap();
//! db.execute("CREATE INDEX idx_email ON users (email)").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'alice', 'a@x.com')").unwrap();
//! db.execute("INSERT INTO users VALUES (2, 'bob', 'b@x.com')").unwrap();
//!
//! let rows = db.query("SELECT id, name FROM users WHERE id = 1").unwrap();
//! assert_eq!(rows.len(), 1);
//! assert_eq!(rows[0][1], "alice");
//! ```

mod wal;
mod sstable;
mod catalog;
mod txn;
mod sql;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use catalog::{Column, ColumnType, IndexDef, Schema, Table};
pub use sql::{QueryResult, Row, Value};
pub use txn::TxnId;

use catalog::Catalog;
use sql::{parse, Statement};
use sstable::SSTable;
use txn::{Txn, TxnState};
use wal::{Wal, WalRecord};

/// Maximum rows kept in the memtable before an automatic flush.
const MEMTABLE_FLUSH_THRESHOLD: usize = 256;

/// Open or create a Cinder database at `path`.
pub struct Database {
    dir: PathBuf,
    wal: Wal,
    catalog: Catalog,
    /// Active memtables keyed by table name. Each is a BTreeMap of primary-key → row.
    memtables: BTreeMap<String, BTreeMap<String, Row>>,
    /// Immutable sorted runs on disk, newest first.
    runs: Vec<SSTable>,
    next_txn: u64,
    active: Option<Txn>,
}

impl Database {
    /// Open (or create) a database directory.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        let wal_path = dir.join("cinder.wal");
        let mut wal = Wal::open(&wal_path)?;

        let catalog_path = dir.join("catalog.bin");
        let mut catalog = if catalog_path.exists() {
            Catalog::load(&catalog_path)?
        } else {
            Catalog::new()
        };

        // Recover WAL into memtables.
        let mut memtables: BTreeMap<String, BTreeMap<String, Row>> = BTreeMap::new();
        let records = wal.recover()?;
        for rec in records {
            apply_wal_record(&mut memtables, &mut catalog, &rec);
        }

        // Load existing sorted runs (newest first by filename).
        let mut runs = Vec::new();
        let mut run_files: Vec<_> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("run-")
                    && e.path().extension().map(|x| x == "sst").unwrap_or(false)
            })
            .collect();
        run_files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        for entry in run_files {
            if let Ok(sst) = SSTable::open(entry.path()) {
                runs.push(sst);
            }
        }

        Ok(Self {
            dir,
            wal,
            catalog,
            memtables,
            runs,
            next_txn: 1,
            active: None,
        })
    }

    /// Execute a single SQL statement (DDL, DML, or transaction control).
    pub fn execute(&mut self, sql: &str) -> io::Result<usize> {
        let stmt = parse(sql).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.exec_stmt(stmt)
    }

    /// Run a SELECT and return the result rows.
    pub fn query(&mut self, sql: &str) -> io::Result<Vec<Row>> {
        let stmt = parse(sql).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        match stmt {
            Statement::Select { .. } => {
                let mut result = QueryResult::default();
                self.exec_select(stmt, &mut result)?;
                Ok(result.rows)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "query() expects a SELECT statement",
            )),
        }
    }

    /// Begin a new transaction.
    pub fn begin(&mut self) -> io::Result<TxnId> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "transaction already active",
            ));
        }
        let id = self.next_txn;
        self.next_txn += 1;
        self.active = Some(Txn::new(id));
        self.wal.append(&WalRecord::Begin { txn_id: id })?;
        Ok(TxnId(id))
    }

    /// Commit the current transaction.
    pub fn commit(&mut self) -> io::Result<()> {
        let txn = self
            .active
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active transaction"))?;
        if txn.state != TxnState::Open {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "transaction is not open",
            ));
        }
        for (table, pk, row) in txn.writes {
            self.wal.append(&WalRecord::Put {
                table: table.clone(),
                pk: pk.clone(),
                row: row.clone(),
            })?;
            self.memtables
                .entry(table)
                .or_default()
                .insert(pk, row);
        }
        for (table, pk) in txn.deletes {
            self.wal.append(&WalRecord::Delete {
                table: table.clone(),
                pk: pk.clone(),
            })?;
            if let Some(mt) = self.memtables.get_mut(&table) {
                mt.remove(&pk);
            }
        }
        self.wal.append(&WalRecord::Commit { txn_id: txn.id })?;
        self.wal.sync()?;
        self.maybe_flush()?;
        Ok(())
    }

    /// Roll back the current transaction.
    pub fn rollback(&mut self) -> io::Result<()> {
        let txn = self
            .active
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active transaction"))?;
        self.wal.append(&WalRecord::Abort { txn_id: txn.id })?;
        let _ = txn;
        Ok(())
    }

    /// Force a memtable flush to a new sorted run.
    pub fn flush(&mut self) -> io::Result<()> {
        self.flush_memtables()
    }

    /// Checkpoint: flush + truncate WAL (after a successful flush).
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.flush_memtables()?;
        self.wal.truncate()?;
        self.catalog.save(&self.dir.join("catalog.bin"))?;
        Ok(())
    }

    fn exec_stmt(&mut self, stmt: Statement) -> io::Result<usize> {
        match stmt {
            Statement::CreateTable { name, columns } => {
                if self.catalog.tables.contains_key(&name) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("table '{}' already exists", name),
                    ));
                }
                let table = Table {
                    name: name.clone(),
                    columns,
                    indexes: Vec::new(),
                };
                self.catalog.tables.insert(name.clone(), table);
                self.wal.append(&WalRecord::CreateTable {
                    name: name.clone(),
                    schema: self.catalog.tables[&name].clone(),
                })?;
                self.catalog.save(&self.dir.join("catalog.bin"))?;
                Ok(0)
            }
            Statement::CreateIndex {
                name,
                table,
                column,
            } => {
                let t = self
                    .catalog
                    .tables
                    .get_mut(&table)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("table '{}' not found", table))
                    })?;
                if t.indexes.iter().any(|i| i.name == name) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("index '{}' already exists", name),
                    ));
                }
                t.indexes.push(IndexDef {
                    name: name.clone(),
                    column: column.clone(),
                });
                self.wal.append(&WalRecord::CreateIndex {
                    name,
                    table,
                    column,
                })?;
                self.catalog.save(&self.dir.join("catalog.bin"))?;
                Ok(0)
            }
            Statement::Insert { table, values } => {
                let schema = self
                    .catalog
                    .tables
                    .get(&table)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("table '{}' not found", table))
                    })?
                    .clone();
                if values.len() != schema.columns.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "column count mismatch",
                    ));
                }
                let pk_idx = schema
                    .columns
                    .iter()
                    .position(|c| c.primary_key)
                    .unwrap_or(0);
                let pk = values[pk_idx].to_string();
                let row: Row = values;

                if let Some(ref mut txn) = self.active {
                    txn.writes.push((table, pk, row));
                    return Ok(1);
                }

                self.wal.append(&WalRecord::Put {
                    table: table.clone(),
                    pk: pk.clone(),
                    row: row.clone(),
                })?;
                self.memtables
                    .entry(table)
                    .or_default()
                    .insert(pk, row);
                self.maybe_flush()?;
                Ok(1)
            }
            Statement::Delete { table, where_clause } => {
                let keys = self.collect_keys(&table, &where_clause)?;
                let mut count = 0;
                for pk in keys {
                    if let Some(ref mut txn) = self.active {
                        txn.deletes.push((table.clone(), pk));
                        count += 1;
                        continue;
                    }
                    self.wal.append(&WalRecord::Delete {
                        table: table.clone(),
                        pk: pk.clone(),
                    })?;
                    if let Some(mt) = self.memtables.get_mut(&table) {
                        if mt.remove(&pk).is_some() {
                            count += 1;
                        }
                    }
                }
                self.maybe_flush()?;
                Ok(count)
            }
            Statement::Update {
                table,
                assignments,
                where_clause,
            } => {
                let keys = self.collect_keys(&table, &where_clause)?;
                let schema = self.catalog.tables.get(&table).cloned().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("table '{}' not found", table))
                })?;
                let mut count = 0;
                for pk in keys {
                    let mut row = self
                        .get_row(&table, &pk)?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "row vanished"))?;
                    for (col, val) in &assignments {
                        if let Some(idx) = schema.columns.iter().position(|c| &c.name == col) {
                            row[idx] = val.clone();
                        }
                    }
                    if let Some(ref mut txn) = self.active {
                        txn.writes.push((table.clone(), pk, row));
                        count += 1;
                        continue;
                    }
                    self.wal.append(&WalRecord::Put {
                        table: table.clone(),
                        pk: pk.clone(),
                        row: row.clone(),
                    })?;
                    self.memtables
                        .entry(table.clone())
                        .or_default()
                        .insert(pk, row);
                    count += 1;
                }
                self.maybe_flush()?;
                Ok(count)
            }
            Statement::Select { .. } => {
                let mut result = QueryResult::default();
                self.exec_select(stmt, &mut result)?;
                Ok(result.rows.len())
            }
            Statement::Begin => {
                self.begin()?;
                Ok(0)
            }
            Statement::Commit => {
                self.commit()?;
                Ok(0)
            }
            Statement::Rollback => {
                self.rollback()?;
                Ok(0)
            }
        }
    }

    fn exec_select(&self, stmt: Statement, out: &mut QueryResult) -> io::Result<()> {
        let Statement::Select {
            columns,
            table,
            where_clause,
            limit,
        } = stmt
        else {
            return Ok(());
        };

        let schema = self.catalog.tables.get(&table).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("table '{}' not found", table))
        })?;

        let proj: Vec<usize> = if columns.len() == 1 && columns[0] == "*" {
            (0..schema.columns.len()).collect()
        } else {
            columns
                .iter()
                .map(|c| {
                    schema
                        .columns
                        .iter()
                        .position(|col| &col.name == c)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown column '{}'", c),
                            )
                        })
                })
                .collect::<io::Result<Vec<_>>>()?
        };

        let mut seen = std::collections::HashSet::new();
        let mut candidates: Vec<(String, Row)> = Vec::new();

        if let Some(mt) = self.memtables.get(&table) {
            for (pk, row) in mt {
                if seen.insert(pk.clone()) {
                    candidates.push((pk.clone(), row.clone()));
                }
            }
        }
        for run in &self.runs {
            for (pk, row) in run.scan(&table)? {
                if seen.insert(pk.clone()) {
                    candidates.push((pk, row));
                }
            }
        }

        for (_pk, row) in candidates {
            if let Some(ref pred) = where_clause {
                if !eval_predicate(pred, &row, schema)? {
                    continue;
                }
            }
            let projected: Row = proj.iter().map(|&i| row[i].clone()).collect();
            out.rows.push(projected);
            if let Some(lim) = limit {
                if out.rows.len() >= lim {
                    break;
                }
            }
        }
        Ok(())
    }

    fn get_row(&self, table: &str, pk: &str) -> io::Result<Option<Row>> {
        if let Some(mt) = self.memtables.get(table) {
            if let Some(r) = mt.get(pk) {
                return Ok(Some(r.clone()));
            }
        }
        for run in &self.runs {
            if let Some(r) = run.get(table, pk)? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    fn collect_keys(
        &self,
        table: &str,
        where_clause: &Option<sql::Predicate>,
    ) -> io::Result<Vec<String>> {
        let schema = self.catalog.tables.get(table).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("table '{}' not found", table))
        })?;
        let mut keys = Vec::new();
        let mut seen = std::collections::HashSet::new();

        if let Some(mt) = self.memtables.get(table) {
            for (pk, row) in mt {
                if seen.insert(pk.clone()) {
                    if let Some(ref pred) = where_clause {
                        if eval_predicate(pred, row, schema)? {
                            keys.push(pk.clone());
                        }
                    } else {
                        keys.push(pk.clone());
                    }
                }
            }
        }
        for run in &self.runs {
            for (pk, row) in run.scan(table)? {
                if seen.insert(pk.clone()) {
                    if let Some(ref pred) = where_clause {
                        if eval_predicate(pred, &row, schema)? {
                            keys.push(pk);
                        }
                    } else {
                        keys.push(pk);
                    }
                }
            }
        }
        Ok(keys)
    }

    fn maybe_flush(&mut self) -> io::Result<()> {
        let total: usize = self.memtables.values().map(|m| m.len()).sum();
        if total >= MEMTABLE_FLUSH_THRESHOLD {
            self.flush_memtables()?;
        }
        Ok(())
    }

    fn flush_memtables(&mut self) -> io::Result<()> {
        if self.memtables.values().all(|m| m.is_empty()) {
            return Ok(());
        }
        let run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = self.dir.join(format!("run-{:020}.sst", run_id));
        let sst = SSTable::build(&path, &self.memtables)?;
        self.runs.insert(0, sst);
        self.memtables.clear();
        if self.runs.len() > 8 {
            self.runs.pop();
        }
        Ok(())
    }
}

fn apply_wal_record(
    memtables: &mut BTreeMap<String, BTreeMap<String, Row>>,
    catalog: &mut Catalog,
    rec: &WalRecord,
) {
    match rec {
        WalRecord::Put { table, pk, row } => {
            memtables
                .entry(table.clone())
                .or_default()
                .insert(pk.clone(), row.clone());
        }
        WalRecord::Delete { table, pk } => {
            if let Some(mt) = memtables.get_mut(table) {
                mt.remove(pk);
            }
        }
        WalRecord::CreateTable { name, schema } => {
            catalog.tables.insert(name.clone(), schema.clone());
        }
        WalRecord::CreateIndex { name, table, column } => {
            if let Some(t) = catalog.tables.get_mut(table) {
                t.indexes.push(IndexDef {
                    name: name.clone(),
                    column: column.clone(),
                });
            }
        }
        WalRecord::Begin { .. } | WalRecord::Commit { .. } | WalRecord::Abort { .. } => {}
    }
}

fn eval_predicate(
    pred: &sql::Predicate,
    row: &Row,
    schema: &Table,
) -> io::Result<bool> {
    match pred {
        sql::Predicate::Eq { column, value } => {
            let idx = schema
                .columns
                .iter()
                .position(|c| &c.name == column)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown column '{}'", column),
                    )
                })?;
            Ok(&row[idx] == value)
        }
        sql::Predicate::Ne { column, value } => {
            let idx = schema
                .columns
                .iter()
                .position(|c| &c.name == column)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown column '{}'", column),
                    )
                })?;
            Ok(&row[idx] != value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("cinder-test-{}", n));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn create_insert_select() {
        let dir = tmp_dir();
        let mut db = Database::open(&dir).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'bob')").unwrap();
        let rows = db.query("SELECT id, name FROM t WHERE id = 1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("alice".into()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transaction_commit() {
        let dir = tmp_dir();
        let mut db = Database::open(&dir).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        db.begin().unwrap();
        db.execute("INSERT INTO t VALUES (10, 'x')").unwrap();
        db.commit().unwrap();
        let rows = db.query("SELECT id, v FROM t WHERE id = 10").unwrap();
        assert_eq!(rows.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transaction_rollback() {
        let dir = tmp_dir();
        let mut db = Database::open(&dir).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        db.begin().unwrap();
        db.execute("INSERT INTO t VALUES (20, 'y')").unwrap();
        db.rollback().unwrap();
        let rows = db.query("SELECT id FROM t WHERE id = 20").unwrap();
        assert!(rows.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovery_after_reopen() {
        let dir = tmp_dir();
        {
            let mut db = Database::open(&dir).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
                .unwrap();
            db.execute("INSERT INTO t VALUES (7, 'recovered')").unwrap();
        }
        let mut db = Database::open(&dir).unwrap();
        let rows = db.query("SELECT name FROM t WHERE id = 7").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Text("recovered".into()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn secondary_index_declared() {
        let dir = tmp_dir();
        let mut db = Database::open(&dir).unwrap();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT)")
            .unwrap();
        db.execute("CREATE INDEX idx_email ON users (email)")
            .unwrap();
        assert!(db.catalog.tables["users"]
            .indexes
            .iter()
            .any(|i| i.name == "idx_email"));
        let _ = fs::remove_dir_all(&dir);
    }
}
