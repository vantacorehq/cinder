//! Length-prefixed CRC32 write-ahead log.
//!
//! Every durable mutation is appended as a framed record and synced.
//! Recovery stops on CRC mismatch or truncated tail.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::catalog::Table;
use crate::sql::{Row, Value};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[derive(Debug, Clone)]
pub enum WalRecord {
    Begin { txn_id: u64 },
    Commit { txn_id: u64 },
    Abort { txn_id: u64 },
    Put {
        table: String,
        pk: String,
        row: Row,
    },
    Delete {
        table: String,
        pk: String,
    },
    CreateTable {
        name: String,
        schema: Table,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
    },
}

impl WalRecord {
    fn tag(&self) -> u8 {
        match self {
            WalRecord::Begin { .. } => 1,
            WalRecord::Commit { .. } => 2,
            WalRecord::Abort { .. } => 3,
            WalRecord::Put { .. } => 4,
            WalRecord::Delete { .. } => 5,
            WalRecord::CreateTable { .. } => 6,
            WalRecord::CreateIndex { .. } => 7,
        }
    }
}

fn encode_str(s: &str, out: &mut Vec<u8>) {
    let b = s.as_bytes();
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn encode_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::Integer(i) => {
            out.push(1);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(2);
            encode_str(s, out);
        }
        Value::Real(f) => {
            out.push(3);
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
}

fn encode_row(row: &Row, out: &mut Vec<u8>) {
    out.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for v in row {
        encode_value(v, out);
    }
}

fn encode_record(rec: &WalRecord) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(rec.tag());
    match rec {
        WalRecord::Begin { txn_id }
        | WalRecord::Commit { txn_id }
        | WalRecord::Abort { txn_id } => {
            body.extend_from_slice(&txn_id.to_le_bytes());
        }
        WalRecord::Put { table, pk, row } => {
            encode_str(table, &mut body);
            encode_str(pk, &mut body);
            encode_row(row, &mut body);
        }
        WalRecord::Delete { table, pk } => {
            encode_str(table, &mut body);
            encode_str(pk, &mut body);
        }
        WalRecord::CreateTable { name, schema } => {
            encode_str(name, &mut body);
            body.extend_from_slice(&(schema.columns.len() as u32).to_le_bytes());
            for c in &schema.columns {
                encode_str(&c.name, &mut body);
                body.push(match c.ty {
                    crate::catalog::ColumnType::Integer => 1,
                    crate::catalog::ColumnType::Text => 2,
                    crate::catalog::ColumnType::Real => 3,
                });
                body.push(if c.primary_key { 1 } else { 0 });
            }
        }
        WalRecord::CreateIndex { name, table, column } => {
            encode_str(name, &mut body);
            encode_str(table, &mut body);
            encode_str(column, &mut body);
        }
    }
    let checksum = crc32(&body);
    let mut frame = Vec::with_capacity(4 + body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let bytes = buf.get(off..end)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u64(buf: &[u8], off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let bytes = buf.get(off..end)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn read_str(buf: &[u8], off: &mut usize) -> Option<String> {
    let len = read_u32(buf, *off)? as usize;
    *off += 4;
    let end = off.checked_add(len)?;
    let s = String::from_utf8_lossy(buf.get(*off..end)?).into_owned();
    *off = end;
    Some(s)
}

fn read_value(buf: &[u8], off: &mut usize) -> Option<Value> {
    let tag = *buf.get(*off)?;
    *off += 1;
    match tag {
        0 => Some(Value::Null),
        1 => {
            let end = off.checked_add(8)?;
            let bytes = buf.get(*off..end)?;
            let i = i64::from_le_bytes(bytes.try_into().ok()?);
            *off = end;
            Some(Value::Integer(i))
        }
        2 => Some(Value::Text(read_str(buf, off)?)),
        3 => {
            let end = off.checked_add(8)?;
            let bytes = buf.get(*off..end)?;
            let f = f64::from_le_bytes(bytes.try_into().ok()?);
            *off = end;
            Some(Value::Real(f))
        }
        _ => None,
    }
}

fn read_row(buf: &[u8], off: &mut usize) -> Option<Row> {
    let n = read_u32(buf, *off)? as usize;
    *off += 4;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        row.push(read_value(buf, off)?);
    }
    Some(row)
}

fn decode_body(body: &[u8]) -> Option<WalRecord> {
    if body.is_empty() {
        return None;
    }
    let tag = body[0];
    let mut off = 1usize;
    match tag {
        1 => {
            let id = read_u64(body, off)?;
            Some(WalRecord::Begin { txn_id: id })
        }
        2 => {
            let id = read_u64(body, off)?;
            Some(WalRecord::Commit { txn_id: id })
        }
        3 => {
            let id = read_u64(body, off)?;
            Some(WalRecord::Abort { txn_id: id })
        }
        4 => {
            let table = read_str(body, &mut off)?;
            let pk = read_str(body, &mut off)?;
            let row = read_row(body, &mut off)?;
            Some(WalRecord::Put { table, pk, row })
        }
        5 => {
            let table = read_str(body, &mut off)?;
            let pk = read_str(body, &mut off)?;
            Some(WalRecord::Delete { table, pk })
        }
        6 => {
            let name = read_str(body, &mut off)?;
            let n = read_u32(body, off)? as usize;
            off += 4;
            let mut columns = Vec::with_capacity(n);
            for _ in 0..n {
                let cname = read_str(body, &mut off)?;
                let ty = match *body.get(off)? {
                    1 => crate::catalog::ColumnType::Integer,
                    2 => crate::catalog::ColumnType::Text,
                    3 => crate::catalog::ColumnType::Real,
                    _ => return None,
                };
                off += 1;
                let pk = *body.get(off)? == 1;
                off += 1;
                columns.push(crate::catalog::Column {
                    name: cname,
                    ty,
                    primary_key: pk,
                });
            }
            Some(WalRecord::CreateTable {
                name: name.clone(),
                schema: Table {
                    name,
                    columns,
                    indexes: Vec::new(),
                },
            })
        }
        7 => {
            let name = read_str(body, &mut off)?;
            let table = read_str(body, &mut off)?;
            let column = read_str(body, &mut off)?;
            Some(WalRecord::CreateIndex {
                name,
                table,
                column,
            })
        }
        _ => None,
    }
}

pub struct Wal {
    file: File,
    path: String,
}

impl Wal {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path_ref = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path_ref)?;
        Ok(Self {
            file,
            path: path_ref.to_string_lossy().into_owned(),
        })
    }

    pub fn append(&mut self, rec: &WalRecord) -> io::Result<()> {
        let frame = encode_record(rec);
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&frame)?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    pub fn recover(&mut self) -> io::Result<Vec<WalRecord>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        self.file.read_to_end(&mut buf)?;
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let len = match read_u32(&buf, off) {
                Some(n) => n as usize,
                None => break,
            };
            let body_start = off + 4;
            let body_end = match body_start.checked_add(len) {
                Some(n) if n + 4 <= buf.len() => n,
                _ => break,
            };
            let stored_crc = match read_u32(&buf, body_end) {
                Some(c) => c,
                None => break,
            };
            let body = &buf[body_start..body_end];
            if crc32(body) != stored_crc {
                break;
            }
            if let Some(rec) = decode_body(body) {
                out.push(rec);
            }
            off = body_end + 4;
        }
        Ok(out)
    }

    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.sync_data()?;
        Ok(())
    }
}
