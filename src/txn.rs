//! Simple transaction support.
//!
//! Writes are buffered until COMMIT. On ROLLBACK they are discarded.
//! The WAL records Begin / Commit / Abort for recovery ordering.

use crate::sql::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Open,
    Committed,
    Aborted,
}

pub struct Txn {
    pub id: u64,
    pub state: TxnState,
    /// Buffered puts: (table, primary key, row)
    pub writes: Vec<(String, String, Row)>,
    /// Buffered deletes: (table, primary key)
    pub deletes: Vec<(String, String)>,
}

impl Txn {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            state: TxnState::Open,
            writes: Vec::new(),
            deletes: Vec::new(),
        }
    }
}
