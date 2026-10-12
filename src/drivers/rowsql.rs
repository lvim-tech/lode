// lode/drivers/rowsql: ONE row write, spelled in each dialect.
//
// "Change these columns where this key matches" is the same thought in every
// SQL dialect, and it was written out by hand in the sqlite driver — where it
// stayed, which is why fourteen of sixteen drivers could not edit a row while
// one could. The two things that actually differ between dialects are how an
// identifier is quoted and how a parameter is spelled, so those are the two
// things this takes.
//
// A name may be an identifier here because it came from the engine's own
// `columns()`; a VALUE never becomes syntax. The binds come back in the order
// their placeholders appear — the SET values first, then the key's — and a
// driver turns them into whatever its library binds.

use crate::spec::{ObjRef, RowPlan, Value};

/// How one dialect spells the two things that differ.
#[derive(Clone, Copy)]
pub struct Spelling {
    /// Wrap an identifier: `"x"`, `` `x` ``, `[x]`.
    pub quote: fn(&str) -> String,
    /// The nth parameter, counting from one: `?`, `$1`, `@P1`.
    pub param: fn(usize) -> String,
}

/// `"name"` — the SQL standard, and what postgres, sqlite, duckdb, firebird
/// and snowflake use. A quote inside a name is doubled.
pub fn quote_double(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `` `name` `` — MySQL and MariaDB. A backtick inside a name is doubled.
pub fn quote_backtick(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// `[name]` — SQL Server. A closing bracket inside a name is doubled.
pub fn quote_bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `?` — positional by order of appearance.
pub fn param_question(_: usize) -> String {
    "?".to_string()
}

/// `$1` — postgres, numbered.
pub fn param_dollar(n: usize) -> String {
    format!("${n}")
}

/// `@P1` — SQL Server, named after its position.
pub fn param_at_p(n: usize) -> String {
    format!("@P{n}")
}

pub const POSTGRES: Spelling = Spelling {
    quote: quote_double,
    param: param_dollar,
};

pub const MYSQL: Spelling = Spelling {
    quote: quote_backtick,
    param: param_question,
};

pub const MSSQL: Spelling = Spelling {
    quote: quote_bracket,
    param: param_at_p,
};

/// sqlite, duckdb, firebird: double quotes and `?`.
pub const ANSI: Spelling = Spelling {
    quote: quote_double,
    param: param_question,
};

/// A bound value in the three shapes a database driver actually binds.
///
/// Text, because what somebody typed IS text and only the server knows what
/// the column is: handed over as text, the server parses it into the type it
/// inferred for that parameter — the same thing it does with a literal, minus
/// any way for the value to become syntax. Guessing the type here is how "12"
/// reaches an integer column as a failed cast.
pub enum Bind {
    Null,
    Text(String),
    /// Decoded ONCE, here, so no driver has to remember that a cell's bytes
    /// arrive base64-encoded.
    Bytes(Vec<u8>),
}

/// Its SHAPE, never its contents. A bound value is the one thing a row-write
/// log leaves out, and a `{:?}` that printed the value would put it back.
impl std::fmt::Debug for Bind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bind::Null => f.write_str("NULL"),
            Bind::Text(_) => f.write_str("text"),
            Bind::Bytes(_) => f.write_str("bytes"),
        }
    }
}

/// What to bind for one value.
pub fn bind_of(v: &Value) -> Bind {
    use base64::Engine as _;
    match v {
        Value::Null => Bind::Null,
        Value::Bool(b) => Bind::Text(b.to_string()),
        Value::Int(i) => Bind::Text(i.to_string()),
        Value::Float(f) => Bind::Text(f.to_string()),
        Value::Text(s) | Value::Timestamp(s) | Value::Oid(s) => Bind::Text(s.clone()),
        Value::Ext { text, .. } => Bind::Text(text.clone()),
        Value::Json(j) => Bind::Text(j.to_string()),
        Value::Bytes { b64, .. } => match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(raw) => Bind::Bytes(raw),
            // Not base64 at all: it was never bytes, so it travels as what it is.
            Err(_) => Bind::Text(b64.clone()),
        },
    }
}

/// `schema.table`, each part quoted, the schema left out when there is none.
pub fn qualify(obj: &ObjRef, quote: fn(&str) -> String) -> String {
    match obj.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", quote(s), quote(&obj.name)),
        None => quote(&obj.name),
    }
}

/// The statement for a row write, and the values to bind to it.
pub fn row_statement(plan: &RowPlan, s: Spelling) -> (String, Vec<Value>) {
    let table = qualify(&plan.obj, s.quote);
    let mut binds: Vec<Value> = Vec::new();
    let stmt = if plan.delete {
        format!("DELETE FROM {table} WHERE {}", where_of(&plan.key, s, &mut binds))
    } else {
        let sets: Vec<String> = plan
            .set
            .iter()
            .map(|(c, v)| {
                binds.push(crate::spec::value_from_json(v));
                format!("{} = {}", (s.quote)(c), (s.param)(binds.len()))
            })
            .collect();
        format!(
            "UPDATE {table} SET {} WHERE {}",
            sets.join(", "),
            where_of(&plan.key, s, &mut binds)
        )
    };
    (stmt, binds)
}

/// The WHERE for an identifying key.
///
/// A NULL in a key is compared with `IS NULL`, because `= NULL` matches nothing
/// — the write would then hit no rows, and the driver would refuse it as
/// "matched 0 rows", correctly and for a reason nobody could see.
fn where_of(key: &[(String, serde_json::Value)], s: Spelling, binds: &mut Vec<Value>) -> String {
    key.iter()
        .map(|(c, v)| {
            let v = crate::spec::value_from_json(v);
            if matches!(v, Value::Null) {
                format!("{} IS NULL", (s.quote)(c))
            } else {
                binds.push(v);
                format!("{} = {}", (s.quote)(c), (s.param)(binds.len()))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(delete: bool) -> RowPlan {
        serde_json::from_value(json!({
            "obj": { "name": "orders", "schema": "shop" },
            "key": [["id", 7]],
            "set": if delete { json!([]) } else { json!([["note", "paid"], ["total", 12.5]]) },
            "delete": delete,
        }))
        .expect("a plan")
    }

    #[test]
    fn each_dialect_writes_its_own_spelling() {
        let (pg, _) = row_statement(&plan(false), POSTGRES);
        assert_eq!(
            pg,
            "UPDATE \"shop\".\"orders\" SET \"note\" = $1, \"total\" = $2 WHERE \"id\" = $3"
        );
        let (my, _) = row_statement(&plan(false), MYSQL);
        assert_eq!(my, "UPDATE `shop`.`orders` SET `note` = ?, `total` = ? WHERE `id` = ?");
        let (ms, _) = row_statement(&plan(false), MSSQL);
        assert_eq!(
            ms,
            "UPDATE [shop].[orders] SET [note] = @P1, [total] = @P2 WHERE [id] = @P3"
        );
    }

    #[test]
    fn the_binds_are_in_the_order_the_placeholders_are() {
        let (_, binds) = row_statement(&plan(false), POSTGRES);
        assert_eq!(binds.len(), 3);
        assert!(matches!(&binds[0], Value::Text(s) if s == "paid"));
        assert!(matches!(binds[1], Value::Float(f) if (f - 12.5).abs() < f64::EPSILON));
        assert!(matches!(binds[2], Value::Int(7)));
    }

    #[test]
    fn a_delete_is_the_key_alone() {
        let (stmt, binds) = row_statement(&plan(true), ANSI);
        assert_eq!(stmt, "DELETE FROM \"shop\".\"orders\" WHERE \"id\" = ?");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn a_null_in_the_key_is_compared_and_not_bound() {
        let p: RowPlan = serde_json::from_value(json!({
            "obj": { "name": "t", "schema": null },
            "key": [["a", null], ["b", 1]],
            "set": [["c", "x"]],
        }))
        .expect("a plan");
        let (stmt, binds) = row_statement(&p, POSTGRES);
        assert_eq!(stmt, "UPDATE \"t\" SET \"c\" = $1 WHERE \"a\" IS NULL AND \"b\" = $2");
        assert_eq!(binds.len(), 2, "the NULL is in the statement, not in the binds");
    }

    #[test]
    fn a_name_cannot_escape_its_quotes() {
        let p: RowPlan = serde_json::from_value(json!({
            "obj": { "name": "ta\"ble", "schema": null },
            "key": [["we`ird", 1]],
            "set": [["br]acket", "x"]],
        }))
        .expect("a plan");
        assert!(row_statement(&p, POSTGRES).0.contains("\"ta\"\"ble\""));
        assert!(row_statement(&p, MYSQL).0.contains("`we``ird`"));
        assert!(row_statement(&p, MSSQL).0.contains("[br]]acket]"));
    }
}
