// lode/split: one buffer of text into the statements it holds.
//
// A SERVER-SIDE job on purpose. Splitting SQL is not splitting on `;`: a
// semicolon inside a string, a comment, a dollar-quoted body or a SQL Server
// batch is not a boundary, and each of those is a fact about a DIALECT. Doing
// it in a client means every client learns Postgres's `$func$` and SQL Server's
// `GO` separately and gets them differently wrong — and this engine already has
// two clients.
//
// What this is NOT: a parser. It does not know what a statement MEANS, only
// where one ends. That is the whole job, and anything more would be a second,
// worse copy of the server's own parser.

/// The statements in `text`, in order, with their surrounding blank space
/// removed and empty pieces dropped.
///
/// `driver` decides the dialect's extras. A driver whose statements are not
/// SQL at all — Mongo's command documents, Redis's command lines — gets the
/// whole text back as ONE statement, because a JSON document full of
/// semicolons is not several commands and splitting it would be destroying it.
pub fn split(driver: &str, text: &str) -> Vec<String> {
    if !is_sql(driver) {
        let t = text.trim();
        return if t.is_empty() { Vec::new() } else { vec![t.to_string()] };
    }
    let go = driver == "mssql";
    let dollar = matches!(driver, "postgres" | "cockroachdb" | "redshift");
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    // Where the current statement began, for the `GO` check: a batch
    // separator is only one when it is alone on its line.
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            // A STRING. Everything to the closing quote is text, including a
            // semicolon, and a doubled quote inside is an escaped quote rather
            // than the end.
            '\'' | '"' | '`' => {
                let quote = c;
                cur.push(c);
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        // `''` inside a string is one quote, not the end.
                        if bytes.get(i + 1) == Some(&quote) {
                            cur.push(quote);
                            cur.push(quote);
                            i += 2;
                            continue;
                        }
                        cur.push(quote);
                        i += 1;
                        break;
                    }
                    // A backslash escape, which MySQL has and the standard
                    // does not. Kept whole either way: a `\'` that is read as
                    // the end of the string makes everything after it a new
                    // statement.
                    if bytes[i] == '\\' && i + 1 < bytes.len() {
                        cur.push('\\');
                        cur.push(bytes[i + 1]);
                        i += 2;
                        continue;
                    }
                    cur.push(bytes[i]);
                    i += 1;
                }
            }
            // A LINE COMMENT, to the end of the line.
            '-' if bytes.get(i + 1) == Some(&'-') => {
                while i < bytes.len() && bytes[i] != '\n' {
                    cur.push(bytes[i]);
                    i += 1;
                }
            }
            // A BLOCK COMMENT. Not nested: the standard says they do not nest,
            // and Postgres says they do — so the closing `*/` ends it, which
            // is what both agree about for the common case.
            '/' if bytes.get(i + 1) == Some(&'*') => {
                cur.push('/');
                cur.push('*');
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == '*' && bytes.get(i + 1) == Some(&'/') {
                        cur.push('*');
                        cur.push('/');
                        i += 2;
                        break;
                    }
                    cur.push(bytes[i]);
                    i += 1;
                }
            }
            // A DOLLAR-QUOTED body: `$$ … $$` or `$tag$ … $tag$`, which is how
            // a function body full of semicolons is written in PostgreSQL.
            '$' if dollar => {
                match dollar_tag(&bytes, i) {
                    Some(tag) => {
                        cur.push_str(&tag);
                        i += tag.chars().count();
                        // Everything to the matching tag, whatever is in it.
                        let tag_chars: Vec<char> = tag.chars().collect();
                        while i < bytes.len() {
                            if bytes[i] == '$' && starts_with(&bytes, i, &tag_chars) {
                                cur.push_str(&tag);
                                i += tag_chars.len();
                                break;
                            }
                            cur.push(bytes[i]);
                            i += 1;
                        }
                    }
                    // A lone `$` — a parameter marker, or money. Not a quote.
                    None => {
                        cur.push('$');
                        i += 1;
                    }
                }
            }
            ';' => {
                // THE BOUNDARY. The semicolon itself is dropped: the client
                // sends each statement on its own and a trailing `;` is noise
                // that some drivers refuse.
                push(&mut out, &cur);
                cur.clear();
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
        // SQL Server's batch separator, checked after each step so it is seen
        // wherever it falls. `GO` is not a statement and not a keyword — it is
        // a word the client tool understands, alone on its line.
        if go && cur.len() >= 2 {
            if let Some(cut) = trailing_go(&cur) {
                let before = cur[..cut].to_string();
                push(&mut out, &before);
                cur.clear();
            }
        }
    }
    push(&mut out, &cur);
    out
}

/// Whether this driver's statements are SQL at all.
///
/// By NAME, which is the one place in the engine where that is right: these
/// are the drivers' own kind strings, and `caps.sql` would be the better
/// question if this function had a driver to ask — `split` is given a name
/// because the client has a name and the registry is not needed to answer.
fn is_sql(driver: &str) -> bool {
    !matches!(driver, "mongodb" | "redis" | "dynamodb" | "s3" | "influxdb" | "cloudwatch")
}

fn push(out: &mut Vec<String>, s: &str) {
    let t = s.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
}

/// The dollar-quote tag starting at `i`, including both `$`s, or `None` when
/// this is not one.
fn dollar_tag(chars: &[char], i: usize) -> Option<String> {
    let mut j = i + 1;
    let mut tag = String::from("$");
    while j < chars.len() {
        let c = chars[j];
        if c == '$' {
            tag.push('$');
            return Some(tag);
        }
        // A tag is a name: letters, digits and underscores. Anything else
        // means this `$` was not a quote.
        if !(c.is_alphanumeric() || c == '_') {
            return None;
        }
        tag.push(c);
        j += 1;
    }
    None
}

fn starts_with(chars: &[char], at: usize, needle: &[char]) -> bool {
    chars.len() >= at + needle.len() && chars[at..at + needle.len()] == *needle
}

/// Where to cut when what has just been written ends with a `GO` on its own
/// line. `None` when it does not.
fn trailing_go(cur: &str) -> Option<usize> {
    let trimmed = cur.trim_end();
    if trimmed.len() < 2 {
        return None;
    }
    let last_line_start = trimmed.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let last = trimmed[last_line_start..].trim();
    if !last.eq_ignore_ascii_case("go") {
        return None;
    }
    // A `GO` on the FIRST line of the buffer with nothing before it is still a
    // separator; it just separates nothing.
    Some(last_line_start)
}

#[cfg(test)]
mod tests {
    use super::split;

    /// The ordinary case: statements separated by semicolons, which are not
    /// kept.
    #[test]
    fn a_buffer_of_statements_comes_apart_at_the_semicolons() {
        let got = split("postgres", "SELECT 1; SELECT 2;\n\nSELECT 3");
        assert_eq!(got, vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
        // Nothing is nothing, not one empty statement.
        assert!(split("postgres", "").is_empty());
        assert!(split("postgres", "   \n  ").is_empty());
        assert!(split("postgres", ";;;").is_empty());
        // One statement stays one, with or without its semicolon.
        assert_eq!(split("postgres", "SELECT 1"), vec!["SELECT 1"]);
        assert_eq!(split("postgres", "  SELECT 1 ;  "), vec!["SELECT 1"]);
    }

    /// A semicolon inside a STRING is not a boundary.
    ///
    /// This is the whole reason splitting is not `text.split(';')`: cutting
    /// here would send `INSERT INTO t VALUES ('a` to the server, which is
    /// either a syntax error or — worse — half a statement that parses.
    #[test]
    fn a_semicolon_inside_a_string_is_text() {
        let got = split("mysql", "INSERT INTO t VALUES ('a;b'); SELECT 1");
        assert_eq!(got, vec!["INSERT INTO t VALUES ('a;b')", "SELECT 1"]);
        // A doubled quote inside a string is an escaped quote, not the end.
        let got = split("postgres", "SELECT 'it''s; here'; SELECT 2");
        assert_eq!(got, vec!["SELECT 'it''s; here'", "SELECT 2"]);
        // A backslash escape, which MySQL has.
        let got = split("mysql", r"SELECT 'a\'; b'; SELECT 2");
        assert_eq!(got, vec![r"SELECT 'a\'; b'", "SELECT 2"]);
        // A quoted IDENTIFIER too — a column really can be called `a;b`.
        let got = split("postgres", "SELECT \"a;b\" FROM t; SELECT 2");
        assert_eq!(got, vec!["SELECT \"a;b\" FROM t", "SELECT 2"]);
        // And MySQL's backticks.
        let got = split("mysql", "SELECT `a;b` FROM t; SELECT 2");
        assert_eq!(got, vec!["SELECT `a;b` FROM t", "SELECT 2"]);
    }

    /// A semicolon inside a COMMENT is not a boundary either.
    #[test]
    fn a_semicolon_inside_a_comment_is_a_comment() {
        let got = split("postgres", "SELECT 1; -- and then; not this\nSELECT 2");
        assert_eq!(got, vec!["SELECT 1", "-- and then; not this\nSELECT 2"]);
        let got = split("postgres", "SELECT 1 /* a; b */ + 2; SELECT 3");
        assert_eq!(got, vec!["SELECT 1 /* a; b */ + 2", "SELECT 3"]);
        // An UNCLOSED comment swallows the rest rather than splitting inside
        // it: the server will say what is wrong with it, and cutting it into
        // pieces would mean a different error about a different thing.
        let got = split("postgres", "SELECT 1; /* never closed; SELECT 2");
        assert_eq!(got, vec!["SELECT 1", "/* never closed; SELECT 2"]);
    }

    /// A dollar-quoted body is one piece, semicolons and all.
    ///
    /// This is how every PL/pgSQL function is written, and splitting inside
    /// one would cut a function body into four statements that mean nothing.
    #[test]
    fn a_function_body_is_not_several_statements() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql; SELECT f()";
        let got = split("postgres", sql);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains("RETURN 1; END;"), "{:?}", got[0]);
        assert_eq!(got[1], "SELECT f()");
        // A TAGGED body, which is what you use when the body itself has `$$`.
        let sql = "CREATE FUNCTION f() AS $body$ SELECT 'a;b'; $body$ LANGUAGE sql; SELECT 2";
        let got = split("postgres", sql);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains("SELECT 'a;b';"), "{:?}", got[0]);
        // A LONE dollar is not a quote: a parameter marker and a money column
        // both carry one, and treating it as a quote would swallow the file.
        let got = split("postgres", "SELECT $1; SELECT 2");
        assert_eq!(got, vec!["SELECT $1", "SELECT 2"]);
        // Dollar quoting is POSTGRES's. In MySQL `$$` is not a quote.
        let got = split("mysql", "SELECT 1; SELECT $$; SELECT 3");
        assert_eq!(got.len(), 3, "{got:?}");
    }

    /// SQL Server's `GO` ends a batch, and it is not a statement.
    #[test]
    fn go_separates_a_batch_in_sql_server() {
        let got = split("mssql", "SELECT 1\nGO\nSELECT 2\nGO");
        assert_eq!(got, vec!["SELECT 1", "SELECT 2"]);
        // Case does not matter, and neither does trailing space.
        let got = split("mssql", "SELECT 1\ngo  \nSELECT 2");
        assert_eq!(got, vec!["SELECT 1", "SELECT 2"]);
        // A `GO` that is NOT alone on its line is not a separator — `SELECT
        // go FROM t` is a column called go.
        let got = split("mssql", "SELECT go FROM t");
        assert_eq!(got, vec!["SELECT go FROM t"]);
        let got = split("mssql", "SELECT 1 go");
        assert_eq!(got, vec!["SELECT 1 go"], "a trailing word is not a separator");
        // And it is SQL SERVER's: in Postgres `GO` is just a word.
        let got = split("postgres", "SELECT 1\nGO\nSELECT 2");
        assert_eq!(got.len(), 1, "{got:?}");
    }

    /// An engine whose statements are not SQL gets its text back whole.
    ///
    /// A Mongo command document is JSON, and a JSON object with semicolons in
    /// its strings is one command. Splitting it would destroy it.
    #[test]
    fn a_document_is_one_statement_however_many_semicolons_it_has() {
        let doc = "{ \"find\": \"t\", \"filter\": { \"a\": \"x;y\" } }";
        assert_eq!(split("mongodb", doc), vec![doc]);
        assert_eq!(split("redis", "SET a \"x;y\""), vec!["SET a \"x;y\""]);
        // Still nothing from nothing.
        assert!(split("mongodb", "  ").is_empty());
    }
}
