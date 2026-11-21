//! Cinder demo — interactive-ish runner and crash-recovery illustration.

use cinder::Database;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = env::args().collect();
    let dir = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from("cinder_data")
    };

    println!("cinder — crash-safe embedded SQL");
    println!("data directory: {}", dir.display());
    println!();

    let mut db = match Database::open(&dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open failed: {}", e);
            std::process::exit(1);
        }
    };

    // Demo sequence
    let statements = [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)",
        "CREATE INDEX idx_email ON users (email)",
        "INSERT INTO users VALUES (1, 'alice', 'alice@example.com')",
        "INSERT INTO users VALUES (2, 'bob', 'bob@example.com')",
        "INSERT INTO users VALUES (3, 'carol', 'carol@example.com')",
        "BEGIN",
        "INSERT INTO users VALUES (4, 'dave', 'dave@example.com')",
        "UPDATE users SET name = 'Alice' WHERE id = 1",
        "COMMIT",
        "SELECT id, name, email FROM users",
        "SELECT name FROM users WHERE id = 2",
        "DELETE FROM users WHERE id = 3",
        "SELECT id, name FROM users",
    ];

    for sql in &statements {
        print!("> {}\n", sql);
        match db.execute(sql) {
            Ok(n) => {
                if sql.starts_with("SELECT") || sql.starts_with("select") {
                    match db.query(sql) {
                        Ok(rows) => {
                            for row in &rows {
                                let cells: Vec<String> =
                                    row.iter().map(|v| v.to_string()).collect();
                                println!("  {}", cells.join(" | "));
                            }
                            println!("  ({} rows)", rows.len());
                        }
                        Err(e) => println!("  query error: {}", e),
                    }
                } else {
                    println!("  ok ({})", n);
                }
            }
            Err(e) => println!("  error: {}", e),
        }
    }

    println!();
    println!("checkpoint…");
    if let Err(e) = db.checkpoint() {
        eprintln!("checkpoint failed: {}", e);
    } else {
        println!("  done");
    }

    println!();
    println!("re-open to prove recovery…");
    drop(db);
    let mut db2 = Database::open(&dir).expect("re-open");
    match db2.query("SELECT id, name FROM users ORDER BY id") {
        Ok(rows) => {
            // ORDER BY not implemented — just show what we have
            for row in &rows {
                let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
                println!("  recovered: {}", cells.join(" | "));
            }
        }
        Err(e) => println!("  recovery query error: {}", e),
    }

    println!();
    println!("cinder ready. Data lives in {}", dir.display());
    let _ = io::stdout().flush();
}
