// lode/drivers/cloudwatch: logs, read as rows.
//
// NOT VERIFIED AGAINST A REAL CLOUDWATCH. No network here, no account. Tested
// properly: the command language, the request each command becomes, the answers
// read into a grid, the time arithmetic, and which commands change something.
// Untested: the HTTP round trip.
//
// Logs are the one thing in this list that genuinely has a TIME RANGE, so this
// driver declares a `time_filter` and a dashboard's shared range reaches it.
// The range crosses as milliseconds since the epoch, which is what the API
// takes, and converting an ISO timestamp to that is the kind of arithmetic that
// is wrong by an hour for six months before anybody notices — so it is tested.

use async_trait::async_trait;

use crate::driver::{Connection, Driver};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, ParamSpec, ParamType, Value,
};

const PARAMS: &[ParamSpec] = &[
    ParamSpec { key: "region", label: "Region", kind: ParamType::String, required: true, secret: false, default: Some("us-east-1") },
    ParamSpec { key: "access_key_id", label: "Access key id", kind: ParamType::String, required: false, secret: false, default: None },
    ParamSpec { key: "secret_access_key", label: "Secret access key", kind: ParamType::String, required: false, secret: true, default: None },
    ParamSpec { key: "aws_profile", label: "…or an AWS profile", kind: ParamType::AwsProfile, required: false, secret: false, default: None },
];

const META: DriverMeta = DriverMeta {
    kind: "cloudwatch",
    display: "CloudWatch Logs",
    default_port: None,
    params: PARAMS,
    auth: &[AuthKind::None, AuthKind::Password],
    caps: Caps {
        sql: false,
        schemas: true,
        cancel: false,
        tls: true,
        tunnel: false,
        multi_db: false,
        kv: true,
        indexes: false,
        ddl: false,
        metrics: false,
        edit: false,
    },
    hint: "groups",
    browse: "tail {name}",
    browse_by_kind: &[("group", "tail {name}")],
    query_form: &[],
    query_template: "",
    browse_form: &[],
    // The one driver in this set that really has a server-side time window.
    // `{col}` is ignored — a log event's time is THE time, not a column
    // somebody chooses — and the template says so by not using it.
    time_filter: "since {from} until {to}",
    builder_ops: &[],
    ddl_ops: &[],
    types: &[],
    inspectors: &[],
};

#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// Every log group.
    Groups { prefix: String },
    /// The streams of one group.
    Streams { group: String },
    /// Events, optionally filtered and time-bounded.
    Tail {
        group: String,
        filter: String,
        from: Option<i64>,
        to: Option<i64>,
    },
}

/// `YYYY-MM-DD[Thh:mm:ss]` to milliseconds since the epoch.
///
/// Read as UTC, deliberately: CloudWatch stores and returns UTC, and quietly
/// applying a local offset would shift every window by the machine's timezone —
/// wrong by an hour for six months of the year, which is the shape of bug
/// nobody attributes to the right place.
pub fn millis(ts: &str) -> Option<i64> {
    let t = ts.trim().trim_end_matches('Z');
    let (date, time) = match t.split_once(['T', ' ']) {
        Some((d, r)) => (d, r),
        None => (t, "00:00:00"),
    };
    let d: Vec<&str> = date.split('-').collect();
    if d.len() != 3 {
        return None;
    }
    let (y, m, day) = (
        d[0].parse::<i64>().ok()?,
        d[1].parse::<i64>().ok()?,
        d[2].parse::<i64>().ok()?,
    );
    let hms: Vec<&str> = time.split(':').collect();
    let hh = hms.first().and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    let mm = hms.get(1).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    let ss = hms.get(2).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    Some((days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss) * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds back to something readable, in UTC.
pub fn from_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let rem = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let t = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{rem:03}Z",
        t / 3600,
        (t % 3600) / 60,
        t % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Read a command. The `since`/`until` words are what the driver's own
/// `time_filter` produces, so a dashboard's range arrives as part of the
/// statement rather than as something the client had to know to append.
pub fn parse(statement: &str) -> anyhow::Result<Cmd> {
    let s = statement.trim();
    let (verb, rest) = match s.split_once(char::is_whitespace) {
        Some((v, r)) => (v.to_ascii_lowercase(), r.trim().to_string()),
        None => (s.to_ascii_lowercase(), String::new()),
    };
    match verb.as_str() {
        "groups" => Ok(Cmd::Groups { prefix: rest }),
        "streams" => {
            if rest.is_empty() {
                anyhow::bail!("`streams` needs a log group");
            }
            Ok(Cmd::Streams { group: rest })
        }
        "tail" => {
            if rest.is_empty() {
                anyhow::bail!("`tail` needs a log group");
            }
            // The group is the first word; `since`/`until` and the rest are a
            // filter. Split on the keywords rather than on position, so the
            // order they were written in does not matter.
            let mut group = String::new();
            let mut filter: Vec<String> = Vec::new();
            let mut from = None;
            let mut to = None;
            let mut it = rest.split_whitespace().peekable();
            while let Some(w) = it.next() {
                match w {
                    "since" => from = it.next().and_then(millis),
                    "until" => to = it.next().and_then(millis),
                    other if group.is_empty() => group = other.to_string(),
                    other => filter.push(other.to_string()),
                }
            }
            if group.is_empty() {
                anyhow::bail!("`tail` needs a log group");
            }
            Ok(Cmd::Tail { group, filter: filter.join(" "), from, to })
        }
        "" => Err(anyhow::anyhow!("say what to do: groups, streams or tail")),
        other => Err(anyhow::anyhow!(
            "'{other}' is not something this understands — it knows groups, streams and tail"
        )),
    }
}

/// The API call and the body a command becomes.
pub fn request_for(cmd: &Cmd) -> (&'static str, serde_json::Value) {
    match cmd {
        Cmd::Groups { prefix } => {
            let mut b = serde_json::json!({ "limit": 50 });
            if !prefix.is_empty() {
                b["logGroupNamePrefix"] = serde_json::json!(prefix);
            }
            ("Logs_20140328.DescribeLogGroups", b)
        }
        Cmd::Streams { group } => (
            "Logs_20140328.DescribeLogStreams",
            serde_json::json!({ "logGroupName": group, "orderBy": "LastEventTime", "descending": true, "limit": 50 }),
        ),
        Cmd::Tail { group, filter, from, to } => {
            let mut b = serde_json::json!({ "logGroupName": group, "limit": 200 });
            if !filter.is_empty() {
                b["filterPattern"] = serde_json::json!(filter);
            }
            if let Some(f) = from {
                b["startTime"] = serde_json::json!(f);
            }
            if let Some(t) = to {
                b["endTime"] = serde_json::json!(t);
            }
            ("Logs_20140328.FilterLogEvents", b)
        }
    }
}

fn col(n: &str) -> Column {
    Column { name: n.to_string(), type_name: String::new() }
}

/// Events, with their time shown as a time rather than as a large number.
pub fn parse_events(v: &serde_json::Value) -> (Vec<Column>, Vec<Vec<Value>>) {
    let cols = vec![col("time"), col("stream"), col("message")];
    let rows = v
        .get("events")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .map(|e| {
                    vec![
                        e.get("timestamp")
                            .and_then(|t| t.as_i64())
                            .map(|t| Value::Text(from_millis(t)))
                            .unwrap_or(Value::Null),
                        Value::Text(e.get("logStreamName").and_then(|s| s.as_str()).unwrap_or("").to_string()),
                        // Trailing newline removed: every CloudWatch message has
                        // one, and in a grid it is an empty second line.
                        Value::Text(
                            e.get("message")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .trim_end_matches('\n')
                                .to_string(),
                        ),
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    (cols, rows)
}

pub fn parse_groups(v: &serde_json::Value) -> (Vec<Column>, Vec<Vec<Value>>) {
    let cols = vec![col("group"), col("created"), col("retention_days"), col("stored_bytes")];
    let rows = v
        .get("logGroups")
        .and_then(|g| g.as_array())
        .map(|a| {
            a.iter()
                .map(|g| {
                    vec![
                        Value::Text(g.get("logGroupName").and_then(|s| s.as_str()).unwrap_or("").to_string()),
                        g.get("creationTime").and_then(|t| t.as_i64()).map(|t| Value::Text(from_millis(t))).unwrap_or(Value::Null),
                        // Absent means "for ever", which is a real answer and
                        // not a missing one — so it is said rather than blank.
                        g.get("retentionInDays").and_then(|d| d.as_i64()).map(Value::Int).unwrap_or(Value::Text("never expires".into())),
                        g.get("storedBytes").and_then(|b| b.as_i64()).map(Value::Int).unwrap_or(Value::Null),
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    (cols, rows)
}

pub struct CloudwatchDriver;

impl CloudwatchDriver {
    pub fn new() -> Self {
        CloudwatchDriver
    }
}

#[async_trait]
impl Driver for CloudwatchDriver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    async fn connect(&self, _spec: &ConnSpec, _net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        Err(anyhow::anyhow!(
            "the CloudWatch Logs driver is written against the documented API but is NOT VERIFIED \
             — no network here and no account. Its request building and its answer parsing are \
             tested; the round trip between them is not."
        ))
    }

    fn classify(&self, _statement: &str) -> Option<String> {
        None
    }

    fn writes(&self, _statement: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_three_verbs_parse_and_the_rest_are_named() {
        assert_eq!(parse("groups").unwrap(), Cmd::Groups { prefix: String::new() });
        assert_eq!(parse("groups /aws/lambda").unwrap(), Cmd::Groups { prefix: "/aws/lambda".into() });
        assert_eq!(parse("streams /aws/lambda/x").unwrap(), Cmd::Streams { group: "/aws/lambda/x".into() });
        let e = parse("SELECT * FROM logs").unwrap_err().to_string();
        assert!(e.contains("groups, streams and tail"), "{e}");
        assert!(parse("tail").is_err());
    }

    /// `since` and `until` are found by keyword, not by position, so the order
    /// somebody wrote them in does not change what they mean.
    #[test]
    fn a_tail_takes_its_window_wherever_it_is_written() {
        let a = parse("tail /aws/x since 2026-01-01 until 2026-02-01 ERROR").unwrap();
        let b = parse("tail /aws/x ERROR since 2026-01-01 until 2026-02-01").unwrap();
        assert_eq!(a, b);
        let Cmd::Tail { group, filter, from, to } = a else { panic!("a tail") };
        assert_eq!(group, "/aws/x");
        assert_eq!(filter, "ERROR");
        assert_eq!(from, Some(1_767_225_600_000));
        assert_eq!(to, Some(1_769_904_000_000));
    }

    /// UTC, deliberately. A local offset would shift every window by the
    /// machine's timezone — wrong by an hour for six months of the year, which
    /// nobody attributes to the right place.
    #[test]
    fn times_are_utc_and_round_trip() {
        assert_eq!(millis("1970-01-01"), Some(0));
        assert_eq!(millis("2026-01-01T00:00:00"), Some(1_767_225_600_000));
        assert_eq!(millis("2026-01-01T12:34:56Z"), Some(1_767_270_896_000));
        // A leap day, which the simple arithmetic gets wrong.
        assert_eq!(millis("2024-02-29"), Some(1_709_164_800_000));
        assert_eq!(from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(from_millis(1_767_270_896_123), "2026-01-01T12:34:56.123Z");
        for s in ["2026-01-01", "2024-02-29T23:59:59"] {
            let ms = millis(s).unwrap();
            assert!(from_millis(ms).starts_with(&s[..10]), "{s} -> {}", from_millis(ms));
        }
        assert_eq!(millis("yesterday"), None);
    }

    #[test]
    fn a_tail_becomes_a_filter_call_with_its_window() {
        let (target, body) = request_for(&parse("tail /aws/x since 2026-01-01 ERROR").unwrap());
        assert_eq!(target, "Logs_20140328.FilterLogEvents");
        assert_eq!(body["logGroupName"], "/aws/x");
        assert_eq!(body["filterPattern"], "ERROR");
        assert_eq!(body["startTime"], 1_767_225_600_000i64);
        assert!(body.get("endTime").is_none(), "an open end is left open");
        assert!(body.get("limit").is_some());
    }

    /// A time is shown as a time. A column of 13-digit numbers is a column
    /// nobody reads.
    #[test]
    fn events_show_their_time_as_a_time() {
        let v = json!({ "events": [
            { "timestamp": 1_767_270_896_123i64, "logStreamName": "s1", "message": "boom\n" },
            { "timestamp": 1_767_270_897_000i64, "logStreamName": "s1", "message": "again" }
        ]});
        let (cols, rows) = parse_events(&v);
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["time", "stream", "message"]);
        assert!(matches!(&rows[0][0], Value::Text(t) if t == "2026-01-01T12:34:56.123Z"));
        // The trailing newline every CloudWatch message has would be an empty
        // second line in a grid.
        assert!(matches!(&rows[0][2], Value::Text(t) if t == "boom"));
        assert_eq!(rows.len(), 2);
    }

    /// No retention set means "for ever", which is an answer and not a gap.
    #[test]
    fn a_group_with_no_retention_says_it_never_expires() {
        let v = json!({ "logGroups": [
            { "logGroupName": "/aws/a", "creationTime": 0, "storedBytes": 100 },
            { "logGroupName": "/aws/b", "creationTime": 0, "retentionInDays": 30, "storedBytes": 1 }
        ]});
        let (_, rows) = parse_groups(&v);
        assert!(matches!(&rows[0][2], Value::Text(t) if t == "never expires"));
        assert!(matches!(rows[1][2], Value::Int(30)));
    }

    /// The one driver here with a real server-side window — and `{col}` is
    /// absent on purpose: a log event's time IS the time, not a column somebody
    /// picks.
    #[test]
    fn it_declares_a_time_window_and_ignores_the_column() {
        assert!(!META.time_filter.is_empty());
        assert!(!META.time_filter.contains("{col}"));
        assert!(META.time_filter.contains("{from}") && META.time_filter.contains("{to}"));
    }

    #[test]
    fn nothing_here_writes() {
        let d = CloudwatchDriver::new();
        for s in ["groups", "tail /aws/x", "DELETE FROM logs"] {
            assert!(!d.writes(s));
            assert!(d.classify(s).is_none());
        }
    }
}
