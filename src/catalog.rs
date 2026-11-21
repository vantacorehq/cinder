//! Table schemas, columns and secondary index definitions.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::sql::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnType {
    Integer,
    Text,
    Real,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub primary_key: bool,
}

#[derive(Debug, Clone)]
pub struct IndexDef {
    pub name: String,
    pub column: String,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub indexes: Vec<IndexDef>,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub tables: BTreeMap<String, Table>,
}

pub type Catalog = Schema;

impl Catalog {
    pub fn new() -> Self {
        Self {
            tables: BTreeMap::new(),
        }
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for (name, table) in &self.tables {
            write_str(&mut buf, name);
            buf.extend_from_slice(&(table.columns.len() as u32).to_le_bytes());
            for c in &table.columns {
                write_str(&mut buf, &c.name);
                buf.push(match c.ty {
                    ColumnType::Integer => 1,
                    ColumnType::Text => 2,
                    ColumnType::Real => 3,
                });
                buf.push(if c.primary_key { 1 } else { 0 });
            }
            buf.extend_from_slice(&(table.indexes.len() as u32).to_le_bytes());
            for idx in &table.indexes {
                write_str(&mut buf, &idx.name);
                write_str(&mut buf, &idx.column);
            }
        }
        let mut f = File::create(path)?;
        f.write_all(&buf)?;
        f.sync_all()?;
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut f = File::open(path)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let mut off = 0usize;
        let n = read_u32(&buf, &mut off)? as usize;
        let mut tables = BTreeMap::new();
        for _ in 0..n {
            let name = read_str(&buf, &mut off)?;
            let ncols = read_u32(&buf, &mut off)? as usize;
            let mut columns = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let cname = read_str(&buf, &mut off)?;
                let ty = match buf[off] {
                    1 => ColumnType::Integer,
                    2 => ColumnType::Text,
                    3 => ColumnType::Real,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "bad column type",
                        ))
                    }
                };
                off += 1;
                let pk = buf[off] == 1;
                off += 1;
                columns.push(Column {
                    name: cname,
                    ty,
                    primary_key: pk,
                });
            }
            let nidx = read_u32(&buf, &mut off)? as usize;
            let mut indexes = Vec::with_capacity(nidx);
            for _ in 0..nidx {
                let iname = read_str(&buf, &mut off)?;
                let col = read_str(&buf, &mut off)?;
                indexes.push(IndexDef {
                    name: iname,
                    column: col,
                });
            }
            tables.insert(
                name.clone(),
                Table {
                    name,
                    columns,
                    indexes,
                },
            );
        }
        Ok(Self { tables })
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
    buf.extend_from_slice(b);
}

fn read_u32(buf: &[u8], off: &mut usize) -> io::Result<u32> {
    if *off + 4 > buf.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "catalog"));
    }
    let v = u32::from_le_bytes(buf[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(v)
}

fn read_str(buf: &[u8], off: &mut usize) -> io::Result<String> {
    let len = read_u32(buf, off)? as usize;
    if *off + len > buf.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "catalog str"));
    }
    let s = String::from_utf8_lossy(&buf[*off..*off + len]).into_owned();
    *off += len;
    Ok(s)
}

/// Helper used by SQL layer when materialising typed values.
pub fn parse_value(s: &str, ty: &ColumnType) -> Value {
    match ty {
        ColumnType::Integer => s
            .parse::<i64>()
            .map(Value::Integer)
            .unwrap_or(Value::Null),
        ColumnType::Real => s
            .parse::<f64>()
            .map(Value::Real)
            .unwrap_or(Value::Null),
        ColumnType::Text => {
            let t = s.trim_matches('\'');
            Value::Text(t.to_string())
        }
    }
}
