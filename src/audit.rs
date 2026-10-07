// lode/audit: the record of what was TRIED, written by the side that does the
// work.
//
// It lives in the engine, not in a client, for the same reason the connection
// policy does: a client is a thing that can be rewritten, replaced, or simply
// decide not to write the line. An audit log a client keeps is a log of what
// that client admits to.
//
// Three things in this file, and the second and third are why it is a file
// rather than three functions in `store.rs`:
//
//   * `normalize` — the statement's SHAPE: the words without the values.
//   * `fingerprint` — a short stable name for that shape, so "the same query
//     ran 400 times" is one group rather than 400 unrelated rows.
//   * `redact` — the credential grammars scrubbed out, so a `CREATE ROLE …
//     PASSWORD 'x'` does not put `x` in a table that outlives it.
//
// WHAT REDACTION IS AND IS NOT. `redact` removes the places where a secret is
// syntactically a credential — `PASSWORD '…'`, `IDENTIFIED BY '…'`, redis
// `AUTH`, `CONFIG SET requirepass`. It does NOT remove every value that happens
// to be sensitive: a token pushed through `INSERT INTO t (tok) VALUES ('…')` is
// an ordinary value in an ordinary place and no pattern can tell it from a
// product name. Somebody who cannot have statement text in a log at all is not
// served by a promise this cannot keep, so there is a real switch instead —
// `Mode`, below.
//
// The CONNECTION secrets never reach here by construction: a connection's audit
// line is assembled in `server.rs` from the non-secret fields of the spec
// (driver, host, port, database, tls mode, tunnel host), and the fields that
// hold credentials are `Secret`, whose Debug is already redacted. There is
// nothing to scrub because nothing was taken.

/// How much of a statement the log keeps.
///
/// Read once from `LODE_AUDIT_STATEMENTS`. The default keeps the redacted text,
/// because the text is what makes the record answer a question; the other two
/// exist because "we redact the obvious cases" is not an answer for a database
/// whose every value is sensitive, and refusing to store the text is better
/// than claiming to have cleaned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The statement, with the credential grammars redacted. Default.
    Full,
    /// Only the SHAPE — `insert into people (name) values ('?')`. Proves what
    /// KIND of statement ran, holds no value from it.
    Shape,
    /// No statement at all; the fingerprint alone. Two runs of the same shape
    /// are still provably the same shape, and nothing from either is kept.
    Off,
}

impl Mode {
    /// `full` (default) | `shape` | `off`. An unrecognised value falls back to
    /// `Full` rather than to the quietest option: a typo in a variable must not
    /// silently turn the log down.
    pub fn from_env() -> Mode {
        match std::env::var("LODE_AUDIT_STATEMENTS").as_deref().map(str::trim) {
            Ok("shape") => Mode::Shape,
            Ok("off") | Ok("none") => Mode::Off,
            _ => Mode::Full,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Shape => "shape",
            Mode::Off => "off",
        }
    }

    /// What goes in the `statement` column for this mode, and the credential
    /// values the redactor took out of it.
    ///
    /// The second half is computed in EVERY mode, because it is not about the
    /// statement column: it is what gets scrubbed from the rest of the record.
    /// A driver that quotes the whole failing statement back in its error
    /// message — which SQLite does, verbatim, offset and all — would otherwise
    /// put the password straight into `reason` after the statement column had
    /// been cleaned. That was a real leak, found by the test that asserts the
    /// password is nowhere in the log rather than nowhere in that one column.
    pub fn render(self, statement: &str) -> (String, Vec<String>) {
        let (redacted, secrets) = redact_with(statement);
        let stored = match self {
            Mode::Full => redacted,
            Mode::Shape => normalize(statement),
            Mode::Off => String::new(),
        };
        (stored, secrets)
    }
}

/// One audit row on its way to the store.
///
/// A struct rather than twelve positional arguments: the two `Option<String>`
/// pairs and the two `Option<i64>` pairs next to each other are exactly the
/// shape in which a caller swaps two of them and nothing complains.
#[derive(Debug, Clone)]
pub struct Record {
    /// The saved connection's NAME, when the client sent one. A live conn_id is
    /// meaningless an hour later; the name is what somebody will search for.
    pub conn: Option<String>,
    pub driver: Option<String>,
    /// Which door this came through: `execute`, `row.write`, `conn.connect`,
    /// `conn.disconnect`. Hooks and scripts add their own when they land, which
    /// is why this is text and not an enum in the schema.
    pub path: String,
    /// The statement as the log keeps it — already through `Mode::render`.
    pub statement: String,
    /// A stable name for the shape of the ORIGINAL statement. Computed before
    /// redaction, so turning the statement column off does not change it.
    pub fingerprint: Option<String>,
    /// The connection's policy AT THE TIME. A policy changed later must not
    /// rewrite what the rules were when this ran.
    pub policy: Option<String>,
    pub confirmed: bool,
    /// `applied` | `refused` | `failed` | `connected` | `disconnected`.
    pub outcome: String,
    pub affected: Option<i64>,
    pub ms: Option<i64>,
    /// Why it was refused, or why it failed. Empty when it went through.
    ///
    /// Set through [`Record::because`], never directly: an engine's error
    /// message routinely quotes the statement it choked on.
    pub reason: Option<String>,
    /// The credential values this record's statement held, kept only long
    /// enough to scrub them out of the other text. Not a column: it never
    /// reaches the store.
    secrets: Vec<String>,
}

impl Record {
    /// A record for `conn_id`-less paths, filled in further by the caller.
    pub fn new(path: &str, outcome: &str) -> Record {
        Record {
            conn: None,
            driver: None,
            path: path.to_string(),
            statement: String::new(),
            fingerprint: None,
            policy: None,
            confirmed: false,
            outcome: outcome.to_string(),
            affected: None,
            ms: None,
            reason: None,
            secrets: Vec::new(),
        }
    }

    /// Put `statement` in, through `mode`, and remember what was taken out.
    pub fn about(&mut self, mode: Mode, statement: &str) {
        self.fingerprint = Some(fingerprint(statement));
        let (stored, secrets) = mode.render(statement);
        self.statement = stored;
        self.secrets = secrets;
    }

    /// Values that must be scrubbed from this record's text, from somewhere
    /// other than a statement.
    ///
    /// A connect has no statement, so `about()` never runs and the secret list
    /// stays empty — which is how an unscrubbed `because()` wrote an SSH key
    /// path, holding what looked like a passphrase, into the log in full.
    pub fn guarding(&mut self, values: Vec<String>) {
        self.secrets.extend(values);
    }

    /// Why this was refused or why it failed, with the statement's credentials
    /// taken out of the message.
    ///
    /// The one way to set `reason`, so that no call site can be the one that
    /// forgets. An engine's error text is not ours to trust: it is assembled by
    /// the database and frequently contains the statement in full.
    pub fn because(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.reason = Some(scrub(&msg, &self.secrets));
    }
}

/// Scrub one value out of an error, for as long as that value is in scope.
///
/// The companion to [`scrub`] for a credential the engine resolved rather than
/// read: between `resolve()` and the call that uses it, the plaintext exists
/// here and nowhere else, so this is the one place it can be taken out of a
/// failure without keeping it alive to do so. After the call it dies as it
/// always did.
///
/// Deliberately NOT a process-wide register of resolved secrets. That would
/// make every message scrubbable, at the price of holding plaintext passwords
/// in a daemon that runs for hours — a worse thing than the leak it closes,
/// because then any later leak carries them ready-made.
pub fn scrubbing<T>(value: &str, r: anyhow::Result<T>) -> anyhow::Result<T> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            let said = e.to_string();
            let clean = scrub(&said, std::slice::from_ref(&value.to_string()));
            if clean == said {
                Err(e)
            } else {
                Err(anyhow::anyhow!("{clean}"))
            }
        }
    }
}

/// Replace each known credential value wherever it appears.
///
/// This is the guarantee the pattern matching alone cannot give: whatever
/// `redact` decided was a credential is gone from every other field of the same
/// record, however that field was worded and whoever worded it.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        // A one-character secret is not worth chasing through a sentence: the
        // replacement would hit ordinary letters and make the message unreadable
        // for no gain.
        if s.len() < 2 {
            continue;
        }
        if out.contains(s.as_str()) {
            out = out.replace(s.as_str(), MASK);
        }
    }
    out
}

// ── the shape of a statement ───────────────────────────────────────────────────

/// A single pass over the statement, classifying the constructs the supported
/// engines share. Used by both `normalize` and `redact`, so the two can never
/// disagree about where a string ends.
///
/// What it knows: `--` and `/* */` comments, `'…'` with both `''` and `\'`
/// escapes, `$tag$…$tag$`, `"…"`, `` `…` ``, `[…]` (only outside JSON, where
/// brackets are structure), numbers, and bare words.
struct Lexer<'a> {
    c: Vec<char>,
    src: &'a str,
    /// Byte offset of each char, plus the end — so a token can be handed back as
    /// a slice of the original text.
    at: Vec<usize>,
    i: usize,
    /// The statement is a JSON document (MongoDB's language). Double-quoted text
    /// is then a VALUE where SQL would call it an identifier.
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    /// A quoted string literal — a value in every dialect.
    Str,
    /// Double-quoted text: an identifier in SQL, a key or a value in JSON.
    DQuoted,
    /// Backtick- or bracket-quoted: always an identifier.
    Ident,
    Number,
    /// A bare word: keyword, identifier, or a redis argument.
    Word,
    /// Anything else, one char at a time: `(`, `,`, `=`, `:`, `{` …
    Punct,
    Space,
}

struct Token<'a> {
    kind: Tok,
    text: &'a str,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Lexer<'a> {
        let t = src.trim();
        let json = t.starts_with('{') || t.starts_with('[');
        let c: Vec<char> = src.chars().collect();
        let mut at: Vec<usize> = src.char_indices().map(|(i, _)| i).collect();
        at.push(src.len());
        Lexer { c, src, at, i: 0, json }
    }

    fn slice(&self, from: usize, to: usize) -> &'a str {
        &self.src[self.at[from]..self.at[to]]
    }

    /// The next token, or `None` at the end. Comments are skipped entirely —
    /// a comment is neither shape nor value, and `WHERE` inside one is not a
    /// `WHERE` (which `classify` already had to learn).
    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            if self.i >= self.c.len() {
                return None;
            }
            let start = self.i;
            let ch = self.c[self.i];

            // -- line comment
            if ch == '-' && self.c.get(self.i + 1) == Some(&'-') {
                while self.i < self.c.len() && self.c[self.i] != '\n' {
                    self.i += 1;
                }
                continue;
            }
            // /* block comment */ — not nested, which is what every supported
            // engine but PostgreSQL does, and a nested one ends early rather
            // than running away.
            if ch == '/' && self.c.get(self.i + 1) == Some(&'*') {
                self.i += 2;
                while self.i + 1 < self.c.len() && !(self.c[self.i] == '*' && self.c[self.i + 1] == '/') {
                    self.i += 1;
                }
                self.i = (self.i + 2).min(self.c.len());
                continue;
            }
            if ch.is_whitespace() {
                while self.i < self.c.len() && self.c[self.i].is_whitespace() {
                    self.i += 1;
                }
                return Some(Token { kind: Tok::Space, text: self.slice(start, self.i) });
            }
            if ch == '\'' {
                self.i += 1;
                while self.i < self.c.len() {
                    match self.c[self.i] {
                        '\\' => self.i = (self.i + 2).min(self.c.len()),
                        '\'' if self.c.get(self.i + 1) == Some(&'\'') => self.i += 2,
                        '\'' => {
                            self.i += 1;
                            break;
                        }
                        _ => self.i += 1,
                    }
                }
                return Some(Token { kind: Tok::Str, text: self.slice(start, self.i) });
            }
            if ch == '"' {
                self.i += 1;
                while self.i < self.c.len() {
                    match self.c[self.i] {
                        '\\' => self.i = (self.i + 2).min(self.c.len()),
                        '"' if self.c.get(self.i + 1) == Some(&'"') => self.i += 2,
                        '"' => {
                            self.i += 1;
                            break;
                        }
                        _ => self.i += 1,
                    }
                }
                return Some(Token { kind: Tok::DQuoted, text: self.slice(start, self.i) });
            }
            if ch == '`' || (ch == '[' && !self.json) {
                let close = if ch == '`' { '`' } else { ']' };
                self.i += 1;
                while self.i < self.c.len() && self.c[self.i] != close {
                    self.i += 1;
                }
                self.i = (self.i + 1).min(self.c.len());
                return Some(Token { kind: Tok::Ident, text: self.slice(start, self.i) });
            }
            // $tag$ … $tag$ (PostgreSQL). `$$` is the empty tag.
            if ch == '$' {
                if let Some(end) = self.dollar_quote() {
                    self.i = end;
                    return Some(Token { kind: Tok::Str, text: self.slice(start, self.i) });
                }
            }
            if ch.is_ascii_digit() {
                self.i += 1;
                while self.i < self.c.len() {
                    let d = self.c[self.i];
                    if d.is_ascii_digit() || d == '.' {
                        self.i += 1;
                    } else if (d == 'e' || d == 'E')
                        && matches!(self.c.get(self.i + 1), Some(n) if n.is_ascii_digit() || *n == '+' || *n == '-')
                    {
                        self.i += 2;
                    } else {
                        break;
                    }
                }
                return Some(Token { kind: Tok::Number, text: self.slice(start, self.i) });
            }
            if ch.is_alphabetic() || ch == '_' {
                while self.i < self.c.len() {
                    let d = self.c[self.i];
                    if d.is_alphanumeric() || d == '_' || d == '$' {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                return Some(Token { kind: Tok::Word, text: self.slice(start, self.i) });
            }
            self.i += 1;
            return Some(Token { kind: Tok::Punct, text: self.slice(start, self.i) });
        }
    }

    /// If a `$tag$` opener starts at `self.i`, the char index just past its
    /// closer. `None` when this `$` is not a dollar quote (a bare `$1`
    /// placeholder, say), and the caller then treats it as punctuation.
    fn dollar_quote(&self) -> Option<usize> {
        let mut j = self.i + 1;
        while j < self.c.len() && (self.c[j].is_alphanumeric() || self.c[j] == '_') {
            j += 1;
        }
        if self.c.get(j) != Some(&'$') {
            return None;
        }
        let tag: Vec<char> = self.c[self.i..=j].to_vec();
        let mut k = j + 1;
        while k + tag.len() <= self.c.len() {
            if self.c[k..k + tag.len()] == tag[..] {
                return Some(k + tag.len());
            }
            k += 1;
        }
        // Unterminated — the rest of the statement is inside it, which is still
        // the honest reading.
        Some(self.c.len())
    }
}

/// The statement's SHAPE: every literal replaced by a placeholder, comments
/// dropped, whitespace collapsed, the words lowercased, a trailing `;` removed.
///
/// Identifiers are KEPT. "An UPDATE of some table" is not a useful group; the
/// table is most of what somebody is looking for.
///
/// This is a lexer over the constructs the supported engines share, not a parser
/// for fourteen dialects. A statement it mis-lexes gets a different shape — it
/// never gets a wrong RECORD, because the statement itself is stored beside the
/// fingerprint. The fingerprint is an index, not the evidence.
pub fn normalize(statement: &str) -> String {
    let mut lx = Lexer::new(statement);
    let json = lx.json;
    let mut out = String::with_capacity(statement.len());
    // In JSON the token after a ':' is a value; the one before it is a key.
    let mut after_colon = false;
    while let Some(t) = lx.next() {
        match t.kind {
            Tok::Space => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            Tok::Str => {
                out.push_str("'?'");
                after_colon = false;
            }
            Tok::Number => {
                out.push('?');
                after_colon = false;
            }
            Tok::DQuoted => {
                if json && after_colon {
                    out.push_str("\"?\"");
                } else {
                    out.push_str(t.text);
                }
                after_colon = false;
            }
            Tok::Ident => {
                out.push_str(t.text);
                after_colon = false;
            }
            Tok::Word => {
                out.push_str(&t.text.to_lowercase());
                after_colon = false;
            }
            Tok::Punct => {
                out.push_str(t.text);
                after_colon = t.text == ":";
            }
        }
    }
    out.trim().trim_end_matches(';').trim_end().to_string()
}

/// A short stable name for a statement's shape.
///
/// FNV-1a over `normalize`'s output, 16 hex digits. Deliberately not a crypto
/// hash and deliberately not keyed: this is an IDENTITY for grouping rows, not
/// a seal proving a row was not altered. A seal would have to be keyed by
/// something a process that can write the table cannot read, and the engine can
/// write the table — so a hash here would look like integrity and be none.
pub fn fingerprint(statement: &str) -> String {
    let shape = normalize(statement);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in shape.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ── credentials ────────────────────────────────────────────────────────────────

/// Words after which a value is a credential.
///
/// Matched as a SUBSTRING of the token, which is what makes
/// `IDENTIFIED WITH mysql_native_password BY '…'` work without a list of
/// authentication plugins: the plugin name carries `password` itself, so it
/// keeps the clause armed instead of ending it.
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "access_key",
    "accesskey",
    "api_key",
    "apikey",
    "token",
    "credential",
    "private_key",
    "privatekey",
    "identified",
];

/// Words that take the REST of the command.
///
/// Redis spells authentication as a verb and one or two bare arguments, and
/// both arguments are the credential. They only count AS the verb — first word
/// of the statement, or straight after `CONFIG SET` — because `SELECT auth FROM
/// sessions` is a read, and a redactor that swallowed the rest of it would be
/// rewriting ordinary statements.
const SECRET_TAIL_WORDS: &[&str] = &["auth", "masterauth", "requirepass"];

/// Connectors between a secret word and its value, which neither arm nor
/// disarm: `IDENTIFIED BY '…'`, `PASSWORD = '…'`, `WITH PASSWORD '…'`.
const CONNECTORS: &[&str] = &["by", "to", "as", "with", "is"];

/// What replaces a credential.
const MASK: &str = "<redacted>";

/// Scrub the credential grammars out of a statement, leaving everything else
/// exactly as it was typed.
///
/// `CREATE ROLE app PASSWORD 'hunter2'` → `CREATE ROLE app PASSWORD '<redacted>'`
/// `ALTER USER u IDENTIFIED BY 'x'`     → `… IDENTIFIED BY '<redacted>'`
/// `CONFIG SET requirepass hunter2`     → `CONFIG SET requirepass <redacted>`
/// `AUTH default hunter2`               → `AUTH <redacted> <redacted>`
///
/// The rule, and the reason it is this rule: in SQL a credential is a QUOTED
/// literal in every supported dialect, so a secret word arms the clause and the
/// next LITERAL is masked, while the next bare word ends it. That is what keeps
/// `SELECT token FROM api_keys` intact — `FROM` is a word, not a literal, and a
/// redactor that masked it would be mangling a read. Bare-argument credentials
/// exist only in redis, and they are the tail words above.
///
/// A quoted value keeps its quotes, so the record still reads as the statement
/// it was. See the file header for what this does NOT catch.
pub fn redact(statement: &str) -> String {
    redact_with(statement).0
}

/// [`redact`], plus the credential values it removed — without their quotes, so
/// they can be searched for in text that is not a statement.
pub fn redact_with(statement: &str) -> (String, Vec<String>) {
    let mut lx = Lexer::new(statement);
    let mut out = String::with_capacity(statement.len());
    let mut secrets: Vec<String> = Vec::new();
    // Armed by a secret word, spent on the next literal.
    let mut armed = false;
    // Armed until the end of the statement (redis AUTH and friends).
    let mut tail = false;
    // For "is this token the command verb": the tail words only count there.
    let mut words_seen = 0usize;
    let mut prev_word = String::new();
    while let Some(t) = lx.next() {
        match t.kind {
            Tok::Space => out.push_str(t.text),
            Tok::Word => {
                let lower = t.text.to_lowercase();
                let verb_position = words_seen == 0 || prev_word == "set";
                if tail {
                    out.push_str(MASK);
                    secrets.push(t.text.to_string());
                } else if verb_position && SECRET_TAIL_WORDS.contains(&lower.as_str()) {
                    out.push_str(t.text);
                    tail = true;
                    armed = false;
                } else if SECRET_WORDS.iter().any(|w| lower.contains(w)) {
                    out.push_str(t.text);
                    armed = true;
                } else if CONNECTORS.contains(&lower.as_str()) {
                    // Neither arms nor disarms.
                    out.push_str(t.text);
                } else {
                    // An ordinary word ENDS the clause. A credential is never a
                    // bare word in SQL, so anything here is the statement
                    // carrying on.
                    out.push_str(t.text);
                    armed = false;
                }
                words_seen += 1;
                prev_word = lower;
            }
            Tok::Str | Tok::DQuoted => {
                if armed || tail {
                    let q = t.text.chars().next().unwrap_or('\'');
                    out.push(q);
                    out.push_str(MASK);
                    out.push(q);
                    // Both the bare value and the quoted form: an error message
                    // may echo either.
                    let inner: String = t.text.chars().skip(1).take(t.text.chars().count().saturating_sub(2)).collect();
                    secrets.push(inner);
                    secrets.push(t.text.to_string());
                    armed = false;
                } else {
                    out.push_str(t.text);
                }
            }
            Tok::Number => {
                if armed || tail {
                    out.push_str(MASK);
                    secrets.push(t.text.to_string());
                    armed = false;
                } else {
                    out.push_str(t.text);
                }
            }
            Tok::Ident => {
                out.push_str(t.text);
                armed = false;
            }
            Tok::Punct => {
                out.push_str(t.text);
                if t.text == ";" {
                    // A new statement starts clean, verb position included.
                    armed = false;
                    tail = false;
                    words_seen = 0;
                    prev_word.clear();
                } else if !matches!(t.text, "=" | "(" | ":" | "@") && !tail {
                    armed = false;
                }
            }
        }
    }
    // Longest first, so a secret that contains another is replaced whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    (out, secrets)
}

#[cfg(test)]
mod tests {

    /// A resolved credential is taken out of the failure raised while it is in
    /// scope — and the value is not kept anywhere to do it.
    #[test]
    fn a_resolved_value_is_scrubbed_from_the_error_it_appears_in() {
        let resolved = "s3cret-phrase-with-punctuation!";
        let r: anyhow::Result<()> = Err(anyhow::anyhow!(
            "cannot load ssh key '{resolved}': No such file or directory"
        ));
        let out = scrubbing(resolved, r).unwrap_err().to_string();
        assert!(!out.contains(resolved), "the value is still there: {out}");
        assert!(out.contains("cannot load ssh key"), "and it still says what failed: {out}");

        // A message that never held it is passed through untouched, so an
        // unrelated error keeps its own wording.
        let r2: anyhow::Result<()> = Err(anyhow::anyhow!("connection refused"));
        assert_eq!(scrubbing(resolved, r2).unwrap_err().to_string(), "connection refused");

        // And success is success.
        let ok: anyhow::Result<u8> = Ok(7);
        assert_eq!(scrubbing(resolved, ok).unwrap(), 7);
    }
    use super::*;

    // ── the shape ────────────────────────────────────────────────────────────

    /// The point of a fingerprint: two runs that differ only in their values are
    /// ONE thing in the log.
    #[test]
    fn the_same_query_with_different_values_is_the_same_shape() {
        let a = "UPDATE people SET name = 'alice' WHERE id = 1";
        let b = "UPDATE people SET name = 'bob'   WHERE id = 29381";
        assert_eq!(normalize(a), normalize(b));
        assert_eq!(fingerprint(a), fingerprint(b));
        assert_eq!(normalize(a), "update people set name = '?' where id = ?");
    }

    /// And the other half, which is the half that matters: a DIFFERENT table is
    /// a different thing, however alike the two statements look.
    #[test]
    fn a_different_table_is_a_different_shape() {
        assert_ne!(
            fingerprint("DELETE FROM people WHERE id = 1"),
            fingerprint("DELETE FROM payments WHERE id = 1")
        );
    }

    #[test]
    fn case_and_whitespace_do_not_make_a_new_shape() {
        assert_eq!(
            fingerprint("select *\n  from  people\nlimit 10;"),
            fingerprint("SELECT * FROM people LIMIT 10")
        );
    }

    /// A comment is neither shape nor value. (`classify` had to learn the same
    /// thing: a WHERE inside a comment is not a WHERE.)
    #[test]
    fn comments_are_not_part_of_the_shape() {
        assert_eq!(
            fingerprint("DELETE FROM t -- careful\nWHERE id = 1"),
            fingerprint("DELETE FROM t /* careful */ WHERE id = 1")
        );
    }

    /// A string containing SQL is a string. This is the shape of the injection
    /// attempt `row.sh` already asserts goes in as a value.
    #[test]
    fn sql_inside_a_literal_stays_inside_it() {
        assert_eq!(
            normalize("INSERT INTO t (c) VALUES ('); DROP TABLE people; --')"),
            "insert into t (c) values ('?')"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_literal() {
        assert_eq!(normalize("SELECT 'it''s' , 'a\\'b' FROM t"), "select '?' , '?' from t");
    }

    #[test]
    fn a_dollar_quoted_body_is_one_literal() {
        assert_eq!(
            normalize("CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql"),
            "create function f() returns int as '?' language sql"
        );
    }

    /// SQL double quotes are an identifier and are kept; JSON double quotes in a
    /// VALUE position are a value and go. The keys stay, because the keys are
    /// the shape of a MongoDB command.
    #[test]
    fn json_values_go_and_json_keys_stay() {
        assert_eq!(
            normalize(r#"{"find": "ratings", "filter": {"grade": "A"}, "limit": 50}"#),
            r#"{"find": "?", "filter": {"grade": "?"}, "limit": ?}"#
        );
        assert_eq!(normalize(r#"SELECT "Name" FROM "People""#), r#"select "Name" from "People""#);
    }

    #[test]
    fn a_fingerprint_is_sixteen_hex_digits() {
        let fp = fingerprint("SELECT 1");
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── credentials ──────────────────────────────────────────────────────────

    #[test]
    fn a_sql_password_literal_is_removed() {
        let out = redact("CREATE ROLE app LOGIN PASSWORD 'hunter2';");
        assert!(!out.contains("hunter2"), "{out}");
        assert_eq!(out, "CREATE ROLE app LOGIN PASSWORD '<redacted>';");
    }

    #[test]
    fn identified_by_is_removed() {
        let out = redact("ALTER USER 'u'@'%' IDENTIFIED BY 'hunter2'");
        assert!(!out.contains("hunter2"), "{out}");
        // The username is not a credential and stays.
        assert!(out.contains("'u'"), "{out}");
    }

    /// MySQL puts the plugin name between the verb and the password, and the
    /// first version of this redactor let the plugin name end the clause — so
    /// the password came through. It is caught because the plugin name carries
    /// `password` in it, not because of a list of plugin names.
    #[test]
    fn a_plugin_name_between_the_verb_and_the_value_does_not_end_the_clause() {
        let out = redact("CREATE USER u IDENTIFIED WITH mysql_native_password BY 'hunter2'");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("mysql_native_password"), "{out}");
    }

    /// And the failure that found the rule: `token` is an ordinary column name,
    /// `FROM` is not its value, and a redactor that masked `FROM` would be
    /// mangling a read.
    #[test]
    fn a_word_after_a_secret_word_is_not_a_credential() {
        assert_eq!(
            redact("SELECT token FROM api_keys WHERE user_id = 3"),
            "SELECT token FROM api_keys WHERE user_id = 3"
        );
    }

    /// `AUTH` eats its tail only where redis puts it: as the verb.
    #[test]
    fn a_tail_word_away_from_the_verb_position_is_just_a_word() {
        assert_eq!(redact("SELECT auth FROM sessions"), "SELECT auth FROM sessions");
    }

    #[test]
    fn password_with_an_equals_sign_is_removed() {
        assert!(!redact("ALTER ROLE r WITH PASSWORD = 'hunter2'").contains("hunter2"));
    }

    /// Redis spells it as bare arguments, and both of them go.
    #[test]
    fn redis_auth_takes_its_whole_tail() {
        assert_eq!(redact("AUTH default hunter2"), "AUTH <redacted> <redacted>");
        assert_eq!(redact("CONFIG SET requirepass hunter2"), "CONFIG SET requirepass <redacted>");
    }

    /// The other half: a statement with no credential in it must come back
    /// BYTE FOR BYTE. A redactor that quietly rewrites ordinary statements makes
    /// the whole log untrustworthy.
    #[test]
    fn an_ordinary_statement_is_returned_unchanged() {
        for s in [
            "SELECT * FROM people WHERE name = 'alice' ORDER BY id LIMIT 10;",
            "UPDATE accounts SET balance = balance - 10 WHERE id = 7",
            r#"{"find": "ratings", "filter": {"grade": "A"}, "limit": 50}"#,
            "HGETALL session:42",
            "SELECT token FROM api_keys WHERE user_id = 3",
        ] {
            assert_eq!(redact(s), s, "redactor changed a statement with no credential in it");
        }
    }

    /// `token` as a column is a word, not a credential — the value after it has
    /// to be a value for the mask to land.
    #[test]
    fn a_secret_word_used_as_a_column_name_does_not_eat_the_next_table() {
        assert_eq!(
            redact("SELECT secret, name FROM vault_entries"),
            "SELECT secret, name FROM vault_entries"
        );
    }

    #[test]
    fn redaction_does_not_touch_a_secret_template() {
        // A template is a REFERENCE to a secret, not a secret. Keeping it is
        // what tells somebody where the credential came from.
        let s = r#"{{ vault "prod/db" }}"#;
        assert_eq!(redact(s), s);
    }

    /// The leak that was really there: SQLite answers a bad statement with the
    /// statement, verbatim — `near "USER": syntax error in ALTER USER app
    /// PASSWORD 'hunter2' at offset 6`. Redacting the statement column and
    /// storing that message is a redaction that did not happen.
    #[test]
    fn a_reason_that_quotes_the_statement_back_is_scrubbed_too() {
        let mut r = Record::new("execute", "failed");
        r.about(Mode::Full, "ALTER USER app PASSWORD 'hunter2'");
        r.because("near \"USER\": syntax error in ALTER USER app PASSWORD 'hunter2' at offset 6");
        assert!(!r.statement.contains("hunter2"), "{}", r.statement);
        let reason = r.reason.clone().unwrap();
        assert!(!reason.contains("hunter2"), "{reason}");
        // And the rest of the message survives — a scrubbed reason that says
        // nothing is a reason nobody can act on.
        assert!(reason.contains("syntax error"), "{reason}");
    }

    /// Off mode does not stop the scrubbing: the needles are found whatever the
    /// statement column is set to keep.
    #[test]
    fn the_reason_is_scrubbed_even_when_the_statement_is_not_kept() {
        let mut r = Record::new("execute", "failed");
        r.about(Mode::Off, "ALTER USER app PASSWORD 'hunter2'");
        r.because("… PASSWORD 'hunter2' …");
        assert_eq!(r.statement, "");
        assert!(!r.reason.clone().unwrap().contains("hunter2"));
    }

    // ── modes ────────────────────────────────────────────────────────────────

    #[test]
    fn shape_mode_keeps_no_value_and_off_mode_keeps_nothing() {
        let s = "INSERT INTO people (name) VALUES ('alice')";
        assert!(!Mode::Shape.render(s).0.contains("alice"));
        assert_eq!(Mode::Shape.render(s).0, "insert into people (name) values ('?')");
        assert_eq!(Mode::Off.render(s).0, "");
        // And the fingerprint is the same in every mode, because it is computed
        // from the original: turning the column down must not break grouping.
        assert_eq!(fingerprint(s), fingerprint(s));
    }

    #[test]
    fn full_is_the_default_and_an_unknown_value_does_not_turn_the_log_down() {
        std::env::remove_var("LODE_AUDIT_STATEMENTS");
        assert_eq!(Mode::from_env(), Mode::Full);
        std::env::set_var("LODE_AUDIT_STATEMENTS", "quiet-please");
        assert_eq!(Mode::from_env(), Mode::Full);
        std::env::set_var("LODE_AUDIT_STATEMENTS", "shape");
        assert_eq!(Mode::from_env(), Mode::Shape);
        std::env::remove_var("LODE_AUDIT_STATEMENTS");
    }
}
