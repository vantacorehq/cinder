//! Immutable sorted runs (SSTables).
//!
//! Simple binary format: sequence of (table, pk, row) records sorted by
//! (table, pk). Used after memtable flush.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::sql::{Row, Value};

pub struct SSTable {
    path: PathBuf,
    /// In-memory index for the current process: table → (pk → offset).
    /// Rebuilt on open. Fine for demo-scale data.
    index: BTreeMap<String, BTreeMap<String, u64>>,
}

impl SSTable {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let mut index: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let mut off = 0usize;
        while off < buf.len() {
            let start = off as u64;
            let table = match read_str(&buf, &mut off) {
                Some(t) => t,
                None => break,
            };
            let pk = match read_str(&buf, &mut off) {
                Some(p) => p,
                None => break,
            };
            if read_row(&buf, &mut off).is_none() {
                break;
            }
            index
                .entry(table)
                .or_default()
                .insert(pk, start);
        }
        Ok(Self { path, index })
    }

    pub fn build<P: AsRef<Path>>(
        path: P,
        memtables: &BTreeMap<String, BTreeMap<String, Row>>,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let mut index: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for (table, rows) in memtables {
            for (pk, row) in rows {
                let offset = file.stream_position()?;
                write_str(&mut file, table)?;
                write_str(&mut file, pk)?;
                write_row(&mut file, row)?;
                index
                    .entry(table.clone())
                    .or_default()
                    .insert(pk.clone(), offset);
            }
        }
        file.sync_all()?;
        Ok(Self { path, index })
    }

    pub fn get(&self, table: &str, pk: &str) -> io::Result<Option<Row>> {
        let offset = match self.index.get(table).and_then(|m| m.get(pk)) {
            Some(&o) => o,
            None => return Ok(None),
        };
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let mut off = 0usize;
        let _ = read_str(&buf, &mut off);
        let _ = read_str(&buf, &mut off);
        Ok(read_row(&buf, &mut off))
    }

    pub fn scan(&self, table: &str) -> io::Result<Vec<(String, Row)>> {
        let mut out = Vec::new();
        if let Some(map) = self.index.get(table) {
            for pk in map.keys() {
                if let Some(row) = self.get(table, pk)? {
                    out.push((pk.clone(), row));
                }
            }
        }
        Ok(out)
    }
}

fn write_str<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    let b = s.as_bytes();
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(b)?;
    Ok(())
}

fn write_value<W: Write>(w: &mut W, v: &Value) -> io::Result<()> {
    match v {
        Value::Null => w.write_all(&[0])?,
        Value::Integer(i) => {
            w.write_all(&[1])?;
            w.write_all(&i.to_le_bytes())?;
        }
        Value::Text(s) => {
            w.write_all(&[2])?;
            write_str(w, s)?;
        }
        Value::Real(f) => {
            w.write_all(&[3])?;
            w.write_all(&f.to_le_bytes())?;
        }
    }
    Ok(())
}

fn write_row<W: Write>(w: &mut W, row: &Row) -> io::Result<()> {
    w.write_all(&(row.len() as u32).to_le_bytes())?;
    for v in row {
        write_value(w, v)?;
    }
    Ok(())
}

fn read_u32(buf: &[u8], off: &mut usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let bytes = buf.get(*off..end)?;
    *off = end;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_str(buf: &[u8], off: &mut usize) -> Option<String> {
    let len = read_u32(buf, off)? as usize;
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
    let n = read_u32(buf, off)? as usize;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        row.push(read_value(buf, off)?);
    }
    Some(row)
}
