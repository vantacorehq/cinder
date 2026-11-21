//! Minimal SQL parser and value types.
//!
//! Supports:
//!   CREATE TABLE name (col TYPE [PRIMARY KEY], ...)
//!   CREATE INDEX name ON table (column)
//!   INSERT INTO table VALUES (v1, v2, ...)
//!   SELECT col, ... FROM table [WHERE col = val] [LIMIT n]
//!   UPDATE table SET col = val, ... [WHERE col = val]
//!   DELETE FROM table [WHERE col = val]
//!   BEGIN | COMMIT | ROLLBACK

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Text(String),
    Real(f64),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(i) => write!(f, "{}", i),
            Value::Text(s) => write!(f, "{}", s),
            Value::Real(r) => write!(f, "{}", r),
        }
    }
}

impl Value {
    pub fn to_string(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Integer(i) => i.to_string(),
            Value::Text(s) => s.clone(),
            Value::Real(r) => r.to_string(),
        }
    }
}

pub type Row = Vec<Value>;

#[derive(Debug, Default)]
pub struct QueryResult {
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone)]
pub enum Predicate {
    Eq { column: String, value: Value },
    Ne { column: String, value: Value },
}

#[derive(Debug)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<crate::catalog::Column>,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
    },
    Insert {
        table: String,
        values: Row,
    },
    Select {
        columns: Vec<String>,
        table: String,
        where_clause: Option<Predicate>,
        limit: Option<usize>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Value)>,
        where_clause: Option<Predicate>,
    },
    Delete {
        table: String,
        where_clause: Option<Predicate>,
    },
    Begin,
    Commit,
    Rollback,
}

/// Parse a single SQL statement. Extremely small hand-rolled lexer.
pub fn parse(input: &str) -> Result<Statement, String> {
    let s = input.trim().trim_end_matches(';');
    let upper = s.to_uppercase();

    if upper == "BEGIN" {
        return Ok(Statement::Begin);
    }
    if upper == "COMMIT" {
        return Ok(Statement::Commit);
    }
    if upper == "ROLLBACK" {
        return Ok(Statement::Rollback);
    }

    if upper.starts_with("CREATE TABLE") {
        return parse_create_table(s);
    }
    if upper.starts_with("CREATE INDEX") {
        return parse_create_index(s);
    }
    if upper.starts_with("INSERT INTO") {
        return parse_insert(s);
    }
    if upper.starts_with("SELECT") {
        return parse_select(s);
    }
    if upper.starts_with("UPDATE") {
        return parse_update(s);
    }
    if upper.starts_with("DELETE FROM") {
        return parse_delete(s);
    }

    Err(format!("unsupported statement: {}", s))
}

fn parse_create_table(s: &str) -> Result<Statement, String> {
    let rest = s["CREATE TABLE".len()..].trim();
    let (name, cols_part) = split_paren(rest)?;
    let name = name.trim().to_string();
    let mut columns = Vec::new();
    for part in cols_part.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() < 2 {
            return Err(format!("bad column definition: {}", part));
        }
        let cname = tokens[0].to_string();
        let ty = match tokens[1].to_uppercase().as_str() {
            "INTEGER" | "INT" => crate::catalog::ColumnType::Integer,
            "TEXT" | "VARCHAR" | "STRING" => crate::catalog::ColumnType::Text,
            "REAL" | "FLOAT" | "DOUBLE" => crate::catalog::ColumnType::Real,
            other => return Err(format!("unknown type '{}'", other)),
        };
        let primary_key = tokens.iter().any(|t| t.eq_ignore_ascii_case("PRIMARY"))
            || tokens.iter().any(|t| t.eq_ignore_ascii_case("KEY"));
        columns.push(crate::catalog::Column {
            name: cname,
            ty,
            primary_key,
        });
    }
    if columns.is_empty() {
        return Err("table must have at least one column".into());
    }
    if !columns.iter().any(|c| c.primary_key) {
        if let Some(c) = columns.iter_mut().find(|c| {
            matches!(c.ty, crate::catalog::ColumnType::Integer)
        }) {
            c.primary_key = true;
        } else {
            columns[0].primary_key = true;
        }
    }
    Ok(Statement::CreateTable { name, columns })
}

fn parse_create_index(s: &str) -> Result<Statement, String> {
    let rest = s["CREATE INDEX".len()..].trim();
    let upper = rest.to_uppercase();
    let on_pos = upper
        .find(" ON ")
        .ok_or_else(|| "expected ON in CREATE INDEX".to_string())?;
    let name = rest[..on_pos].trim().to_string();
    let after_on = rest[on_pos + 4..].trim();
    let (table, col_part) = split_paren(after_on)?;
    let table = table.trim().to_string();
    let column = col_part.trim().to_string();
    if column.is_empty() {
        return Err("index column required".into());
    }
    Ok(Statement::CreateIndex {
        name,
        table,
        column,
    })
}

fn parse_insert(s: &str) -> Result<Statement, String> {
    let rest = s["INSERT INTO".len()..].trim();
    let upper = rest.to_uppercase();
    let values_pos = upper
        .find("VALUES")
        .ok_or_else(|| "expected VALUES".to_string())?;
    let table = rest[..values_pos].trim().to_string();
    let after = rest[values_pos + 6..].trim();
    let (_, vals_str) = split_paren(after)?;
    let values = parse_value_list(vals_str)?;
    Ok(Statement::Insert { table, values })
}

fn parse_select(s: &str) -> Result<Statement, String> {
    let rest = s["SELECT".len()..].trim();
    let upper = rest.to_uppercase();
    let from_pos = upper
        .find(" FROM ")
        .ok_or_else(|| "expected FROM".to_string())?;
    let cols_str = rest[..from_pos].trim();
    let columns: Vec<String> = if cols_str == "*" {
        vec!["*".into()]
    } else {
        cols_str
            .split(',')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect()
    };
    let after_from = rest[from_pos + 6..].trim();
    let (table, where_clause, limit) = parse_where_limit(after_from)?;
    Ok(Statement::Select {
        columns,
        table,
        where_clause,
        limit,
    })
}

fn parse_update(s: &str) -> Result<Statement, String> {
    let rest = s["UPDATE".len()..].trim();
    let upper = rest.to_uppercase();
    let set_pos = upper
        .find(" SET ")
        .ok_or_else(|| "expected SET".to_string())?;
    let table = rest[..set_pos].trim().to_string();
    let after_set = rest[set_pos + 5..].trim();
    let (assign_str, where_clause, _) = {
        let u = after_set.to_uppercase();
        if let Some(w) = u.find(" WHERE ") {
            let assign = after_set[..w].trim();
            let (t, pred, lim) = parse_where_limit(&format!("x {}", &after_set[w..]))?;
            let _ = t;
            (assign, pred, lim)
        } else {
            (after_set, None, None)
        }
    };
    let mut assignments = Vec::new();
    for part in assign_str.split(',') {
        let part = part.trim();
        if let Some(eq) = part.find('=') {
            let col = part[..eq].trim().to_string();
            let val = parse_literal(part[eq + 1..].trim())?;
            assignments.push((col, val));
        }
    }
    Ok(Statement::Update {
        table,
        assignments,
        where_clause,
    })
}

fn parse_delete(s: &str) -> Result<Statement, String> {
    let rest = s["DELETE FROM".len()..].trim();
    let (table, where_clause, _) = parse_where_limit(rest)?;
    Ok(Statement::Delete {
        table,
        where_clause,
    })
}

fn parse_where_limit(
    s: &str,
) -> Result<(String, Option<Predicate>, Option<usize>), String> {
    let upper = s.to_uppercase();
    let (table_part, rest) = if let Some(w) = upper.find(" WHERE ") {
        (&s[..w], Some(&s[w + 7..]))
    } else if let Some(l) = upper.find(" LIMIT ") {
        (&s[..l], Some(&s[l..]))
    } else {
        (s, None)
    };
    let table = table_part.trim().to_string();
    let mut where_clause = None;
    let mut limit = None;
    if let Some(r) = rest {
        let r = r.trim();
        let ru = r.to_uppercase();
        if ru.starts_with("WHERE ") || (!ru.starts_with("LIMIT ") && r.contains('=')) {
            let pred_str = if ru.starts_with("WHERE ") {
                r[6..].trim()
            } else {
                r
            };
            let (pred_part, after_pred) = if let Some(l) = pred_str.to_uppercase().find(" LIMIT ") {
                (&pred_str[..l], Some(&pred_str[l + 7..]))
            } else {
                (pred_str, None)
            };
            where_clause = Some(parse_predicate(pred_part.trim())?);
            if let Some(lim_str) = after_pred {
                limit = Some(
                    lim_str
                        .trim()
                        .parse()
                        .map_err(|_| "bad LIMIT".to_string())?,
                );
            }
        } else if ru.starts_with("LIMIT ") {
            limit = Some(
                r[6..]
                    .trim()
                    .parse()
                    .map_err(|_| "bad LIMIT".to_string())?,
            );
        }
    }
    Ok((table, where_clause, limit))
}

fn parse_predicate(s: &str) -> Result<Predicate, String> {
    if let Some(pos) = s.find("!=") {
        let col = s[..pos].trim().to_string();
        let val = parse_literal(s[pos + 2..].trim())?;
        return Ok(Predicate::Ne {
            column: col,
            value: val,
        });
    }
    if let Some(pos) = s.find('=') {
        let col = s[..pos].trim().to_string();
        let val = parse_literal(s[pos + 1..].trim())?;
        return Ok(Predicate::Eq {
            column: col,
            value: val,
        });
    }
    Err(format!("unsupported predicate: {}", s))
}

fn parse_value_list(s: &str) -> Result<Row, String> {
    let mut values = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for ch in s.chars() {
        match ch {
            '\'' if !in_str => {
                in_str = true;
                cur.push(ch);
            }
            '\'' if in_str => {
                in_str = false;
                cur.push(ch);
            }
            ',' if !in_str => {
                values.push(parse_literal(cur.trim())?);
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        values.push(parse_literal(cur.trim())?);
    }
    Ok(values)
}

fn parse_literal(s: &str) -> Result<Value, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("NULL") {
        return Ok(Value::Null);
    }
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        return Ok(Value::Text(s[1..s.len() - 1].to_string()));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Value::Integer(i));
    }
    if let Ok(f) = s.parse::<f64>() {
        return Ok(Value::Real(f));
    }
    Ok(Value::Text(s.to_string()))
}

fn split_paren(s: &str) -> Result<(&str, &str), String> {
    let start = s
        .find('(')
        .ok_or_else(|| "expected '('".to_string())?;
    let end = s
        .rfind(')')
        .ok_or_else(|| "expected ')'".to_string())?;
    if end <= start {
        return Err("bad parentheses".into());
    }
    Ok((&s[..start], &s[start + 1..end]))
}
