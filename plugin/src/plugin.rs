//! The sensors Plugin: every sensor's reading on one stream, the latest as a snapshot, the recent values of
//! one, and occurrences when a threshold has held long enough or the kernel signals.
//!
//! - **The sensors are read at an interval the application sets**, because a reading is what the domain
//!   is. Each reading is counted, whether or not the stream flows: its `sequence`. `sensors.readings` is
//!   emitted only while the Core has it activated.
//! - **`sensors.snapshot` is the latest reading**, with its sequence. An interface that attaches late
//!   subscribes to the stream, then asks the snapshot, and drops what the stream brings at or below it.
//! - **A threshold is a sensor's own limit**, or the one the application sets among the unit's arguments.
//!   A reading at or above it is `high` or `critical` at once, and back to `normal` only once it has come
//!   back below it by the hysteresis; the occurrence is reported once the level has held for
//!   `--for-seconds`, and `sensors.threshold.cleared` once it is normal again.
//! - **What the kernel signals is not read again to be found**: an hwmon alarm attribute its driver
//!   notifies is `sensors.alarm`, a thermal zone crossing a trip point is `sensors.trip`.
//! - **Every occurrence carries its facts as JSON in its detail**, which is what an interface client
//!   receives; the line is for the log.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use yoke_sdk::plugin::{Capability, Declaration, Event, Object, Question, Severity, Stream, Unit};

use crate::backend::{Kind, SensorBackend, Signal, Survey};

pub const PLUGIN: &str = "com.yoke.sensors";
/// The format every answer, sample and detail states as `schema`.
pub const SCHEMA: u32 = 1;

/// The latest reading, with its sequence, what was found and what is followed.
pub const SNAPSHOT: &str = "sensors.snapshot";
/// The recent values of one sensor.
pub const HISTORY: &str = "sensors.history";
pub const QUERIES: [&str; 2] = [SNAPSHOT, HISTORY];

/// Every sensor, at every reading.
pub const READINGS: &str = "sensors.readings";
pub const STREAMS: [&str; 1] = [READINGS];

pub const HIGH: &str = "sensors.threshold.high";
pub const CRITICAL: &str = "sensors.threshold.critical";
pub const CLEARED: &str = "sensors.threshold.cleared";
/// An hwmon alarm attribute raised or cleared, as the kernel signalled it.
pub const ALARM: &str = "sensors.alarm";
/// A thermal zone crossed a trip point, as the kernel signalled it.
pub const TRIP: &str = "sensors.trip";
pub const OCCURRENCES: [&str; 5] = [HIGH, CRITICAL, CLEARED, ALARM, TRIP];

/// The values kept of each sensor for `sensors.history`.
pub const HISTORY_CAP: usize = 900;
/// No payload is larger: a reading of more sensors than fit is cut, and says so.
pub const MAX_PAYLOAD: usize = 1024 * 1024 - 4096;

pub fn declaration() -> Declaration {
    let answer = |q: &&str| Capability { name: format!("query.{q}.answer"), governs: Object::Query(q.to_string()) };
    let publish = |s: &&str| Capability { name: format!("stream.{s}.publish"), governs: Object::Stream(s.to_string()) };
    let report = |o: &&str| Capability { name: format!("event.{o}.report"), governs: Object::Occurrence(o.to_string()) };
    Declaration {
        id: PLUGIN.into(),
        queries: QUERIES.iter().map(|q| q.to_string()).collect(),
        // Each reading is the whole picture, so a lost one is made good by the next.
        streams: vec![Stream { id: READINGS.into(), tolerates_loss: true, tolerates_reorder: false, max_frame: 0 }],
        occurrences: OCCURRENCES.iter().map(|o| o.to_string()).collect(),
        capabilities: QUERIES.iter().map(answer).chain(STREAMS.iter().map(publish)).chain(OCCURRENCES.iter().map(report)).collect(),
        ..Default::default()
    }
}

/// What the application sets among the unit's arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// How often every sensor is read.
    pub interval_ms: u64,
    /// How long a level must hold before it is reported.
    pub for_seconds: u64,
    /// How far a temperature must come back below a threshold to leave it.
    pub hysteresis_celsius: f64,
    /// The thresholds of every temperature that states none of its own.
    pub high_celsius: Option<f64>,
    pub critical_celsius: Option<f64>,
    /// The thresholds of one sensor, by id, over its own.
    pub high: BTreeMap<String, f64>,
    pub critical: BTreeMap<String, f64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_ms: 2000,
            for_seconds: 10,
            hysteresis_celsius: 2.0,
            high_celsius: None,
            critical_celsius: None,
            high: BTreeMap::new(),
            critical: BTreeMap::new(),
        }
    }
}

fn per_sensor(v: Option<&str>) -> Option<(String, f64)> {
    let (id, value) = v?.rsplit_once('=')?;
    Some((id.to_string(), value.parse().ok()?))
}

impl Config {
    /// `--interval-ms N` (at least 100), `--for-seconds N`, `--hysteresis-celsius C`, `--high-celsius C`,
    /// `--critical-celsius C`, and `--high ID=V` and `--critical ID=V` as often as wanted; anything else,
    /// or a value that does not read, leaves the default.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut c = Config::default();
        let mut args = args.into_iter();
        while let Some(a) = args.next() {
            let value = args.next();
            let value = value.as_deref();
            let number = |d: f64| value.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(d);
            match a.as_str() {
                "--interval-ms" => c.interval_ms = value.and_then(|v| v.parse().ok()).filter(|n| *n >= 100).unwrap_or(c.interval_ms),
                "--for-seconds" => c.for_seconds = value.and_then(|v| v.parse().ok()).unwrap_or(c.for_seconds),
                "--hysteresis-celsius" => c.hysteresis_celsius = number(c.hysteresis_celsius).max(0.0),
                "--high-celsius" => c.high_celsius = value.and_then(|v| v.parse().ok()).or(c.high_celsius),
                "--critical-celsius" => c.critical_celsius = value.and_then(|v| v.parse().ok()).or(c.critical_celsius),
                "--high" => c.high.extend(per_sensor(value)),
                "--critical" => c.critical.extend(per_sensor(value)),
                _ => {}
            }
        }
        c
    }

    /// How many readings in a row make the duration.
    pub fn readings(&self) -> u32 {
        (self.for_seconds * 1000).div_ceil(self.interval_ms).max(1) as u32
    }

    /// A sensor's thresholds: the application's for it, else its own, else the application's for every
    /// temperature. None for a kind where more is not worse.
    pub fn limits(&self, s: &crate::backend::Sensor) -> (Option<f64>, Option<f64>) {
        if !s.kind.has_limits() {
            return (None, None);
        }
        let all = |c: Option<f64>| if s.kind == Kind::Temperature { c } else { None };
        (
            self.high.get(&s.id).copied().or(s.max).or(all(self.high_celsius)),
            self.critical.get(&s.id).copied().or(s.crit).or(all(self.critical_celsius)),
        )
    }

    fn hysteresis(&self, kind: Kind, threshold: f64) -> f64 {
        match kind {
            Kind::Temperature => self.hysteresis_celsius,
            _ => (threshold.abs() * 0.03).max(1e-6),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Normal,
    High,
    Critical,
}

/// The next level of a value. Entering a level needs the value at or above its threshold; leaving it needs
/// the value below the threshold by the hysteresis, so that a value at the threshold does not flap.
pub fn evaluate(prev: Level, v: f64, high: Option<f64>, crit: Option<f64>, hyst: impl Fn(f64) -> f64) -> Level {
    let at = |t: Option<f64>| t.is_some_and(|t| v >= t);
    let near = |t: Option<f64>| t.is_some_and(|t| v >= t - hyst(t));
    match prev {
        Level::Critical if near(crit) => Level::Critical,
        Level::Critical | Level::High if at(crit) => Level::Critical,
        Level::Critical | Level::High if near(high) => Level::High,
        Level::Critical | Level::High => Level::Normal,
        Level::Normal if at(crit) => Level::Critical,
        Level::Normal if at(high) => Level::High,
        Level::Normal => Level::Normal,
    }
}

/// A sensor's level, how long it has held, and what was last reported of it.
#[derive(Debug, Default)]
struct Track {
    level: Level,
    held_high: u32,
    held_critical: u32,
    reported: Level,
}

impl Track {
    /// One value: the level reported now, where one is.
    fn step(&mut self, v: f64, (high, crit): (Option<f64>, Option<f64>), hyst: impl Fn(f64) -> f64, needed: u32) -> Option<Level> {
        self.level = evaluate(self.level, v, high, crit, hyst);
        self.held_high = if self.level >= Level::High { self.held_high + 1 } else { 0 };
        self.held_critical = if self.level == Level::Critical { self.held_critical + 1 } else { 0 };
        if self.held_critical >= needed && self.reported < Level::Critical {
            self.reported = Level::Critical;
            return Some(Level::Critical);
        }
        if self.held_high >= needed && self.reported < Level::High {
            self.reported = Level::High;
            return Some(Level::High);
        }
        if self.level == Level::Normal && self.reported > Level::Normal {
            self.reported = Level::Normal;
            return Some(Level::Normal);
        }
        // Down from critical to high: critical is armed again, and high was said.
        if self.level < self.reported {
            self.reported = self.level;
        }
        None
    }
}

#[derive(Debug)]
pub enum Error {
    Invalid(String),
    NotFound(String),
}

pub fn code(e: &Error) -> &'static str {
    match e {
        Error::Invalid(_) => "invalid",
        Error::NotFound(_) => "not_found",
    }
}

pub fn message(e: &Error) -> String {
    match e {
        Error::Invalid(m) | Error::NotFound(m) => m.clone(),
    }
}

fn input<T: DeserializeOwned>(what: &str, payload: &[u8]) -> Result<T, Error> {
    let payload = if payload.iter().all(u8::is_ascii_whitespace) { b"{}".as_slice() } else { payload };
    serde_json::from_slice(payload).map_err(|e| {
        let kind = match e.classify() {
            serde_json::error::Category::Data => "a field is missing, unknown or of the wrong type",
            _ => "it is not JSON",
        };
        Error::Invalid(format!("the payload is not what {what} takes: {kind}, at line {} column {}", e.line(), e.column()))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NoInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryInput {
    /// A sensor's id, as the snapshot names it.
    pub id: String,
    /// Only the last `n` values.
    #[serde(default)]
    pub n: Option<usize>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub trait Acts {
    fn answer(&self, q: &Question, payload: Vec<u8>) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn fail(&self, q: &Question, code: &str, message: &str) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    /// An occurrence, its facts as JSON in its detail.
    fn report(&self, occurrence: &str, severity: u8, line: &str, detail: Value) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn emit(&self, stream: &str, payload: Vec<u8>) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
}

impl Acts for Unit {
    async fn answer(&self, q: &Question, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        Unit::answer(self, q, payload).await
    }
    async fn fail(&self, q: &Question, code: &str, message: &str) -> Result<(), yoke_sdk::base::Error> {
        Unit::fail(self, &q.id, code, message).await
    }
    async fn report(&self, occurrence: &str, severity: u8, line: &str, detail: Value) -> Result<(), yoke_sdk::base::Error> {
        Unit::report(self, occurrence, Some(Severity::of(severity)), line, detail.to_string().into_bytes()).await
    }
    async fn emit(&self, stream: &str, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        Unit::emit(self, stream, payload).await
    }
}

/// The first items that fit in `budget` bytes of JSON, and whether any was left out.
pub fn fit(items: &[Value], budget: usize) -> (Vec<Value>, bool) {
    let mut used = 2;
    for (i, v) in items.iter().enumerate() {
        used += v.to_string().len() + 1;
        if used > budget {
            return (items[..i].to_vec(), true);
        }
    }
    (items.to_vec(), false)
}

fn number(v: f64) -> String {
    format!("{v:.1}")
}

/// A threshold reached, to be reported once the reading has been emitted.
struct Crossing {
    level: Level,
    sensor: String,
    name: String,
    value: f64,
    unit: &'static str,
    threshold: Option<f64>,
}

/// A sensor's unit and its recent values, `(at_ms, value)`.
type History = (&'static str, VecDeque<(u64, f64)>);

/// The sensors as last read, what each has held, and whether the stream flows.
pub struct Sensors {
    config: Config,
    backend: Arc<dyn SensorBackend>,
    flows: bool,
    sequence: u64,
    at_ms: u64,
    readings: Vec<Value>,
    sources: BTreeMap<&'static str, usize>,
    tracks: HashMap<String, Track>,
    /// Each sensor's unit and recent values.
    history: HashMap<String, History>,
    /// Each sensor's device and label, for the lines of what the kernel signals.
    names: HashMap<String, String>,
    said: Option<(u8, String)>,
}

impl Sensors {
    pub fn new(config: Config, backend: Arc<dyn SensorBackend>) -> Sensors {
        Sensors {
            config,
            backend,
            flows: false,
            sequence: 0,
            at_ms: 0,
            readings: Vec::new(),
            sources: BTreeMap::new(),
            tracks: HashMap::new(),
            history: HashMap::new(),
            names: HashMap::new(),
            said: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn backend(&self) -> Arc<dyn SensorBackend> {
        self.backend.clone()
    }

    /// The latest reading: what the stream carries and the snapshot starts from.
    fn record(&self) -> Value {
        let (sensors, truncated) = fit(&self.readings, MAX_PAYLOAD - 512);
        json!({ "schema": SCHEMA, "sequence": self.sequence, "at_ms": self.at_ms, "truncated": truncated, "sensors": sensors })
    }

    /// One reading of every sensor: counted, kept, emitted where the stream flows, and the thresholds it
    /// reached reported once they have held.
    pub async fn tick(&mut self, u: &impl Acts, survey: Survey) -> Result<(), yoke_sdk::base::Error> {
        self.sequence += 1;
        self.at_ms = now_ms();
        let alarms = self.backend.alarms();
        let needed = self.config.readings();
        let mut crossings = Vec::new();
        let mut readings = Vec::with_capacity(survey.sensors.len());
        let live: HashSet<&str> = survey.sensors.iter().map(|s| s.id.as_str()).collect();
        self.tracks.retain(|id, _| live.contains(id.as_str()));
        self.history.retain(|id, _| live.contains(id.as_str()));
        self.names.retain(|id, _| live.contains(id.as_str()));
        for s in &survey.sensors {
            let limits = self.config.limits(s);
            let name = format!("{} {}", s.device, s.label);
            let track = self.tracks.entry(s.id.clone()).or_default();
            if let (Some(v), true) = (s.value, s.kind.has_limits()) {
                let config = &self.config;
                if let Some(level) = track.step(v, limits, |t| config.hysteresis(s.kind, t), needed) {
                    let threshold = match level {
                        Level::Critical => limits.1,
                        _ => limits.0.or(limits.1),
                    };
                    crossings.push(Crossing { level, sensor: s.id.clone(), name: name.clone(), value: v, unit: s.kind.unit(), threshold });
                }
            }
            if let Some(v) = s.value {
                let (_, h) = self.history.entry(s.id.clone()).or_insert_with(|| (s.kind.unit(), VecDeque::new()));
                if h.len() == HISTORY_CAP {
                    h.pop_front();
                }
                h.push_back((self.at_ms, v));
            }
            let mut r = json!({
                "id": s.id, "source": s.source, "device": s.device, "kind": s.kind, "label": s.label,
                "unit": s.kind.unit(), "value": s.value, "high": limits.0, "critical": limits.1,
                "level": s.kind.has_limits().then_some(track.level),
                "alarm": alarms.get(&s.id).cloned().unwrap_or_default(),
            });
            if let Some(e) = &s.error {
                r["error"] = json!(e);
            }
            readings.push(r);
            self.names.insert(s.id.clone(), name);
        }
        self.readings = readings;
        self.sources = survey.sources();
        let mut first = Ok(());
        if self.flows {
            first = u.emit(READINGS, self.record().to_string().into_bytes()).await;
        }
        for c in crossings {
            first = first.and(self.crossed(u, c).await);
        }
        first
    }

    async fn crossed(&self, u: &impl Acts, c: Crossing) -> Result<(), yoke_sdk::base::Error> {
        let (occurrence, severity) = match c.level {
            Level::Critical => (CRITICAL, 70),
            Level::High => (HIGH, 50),
            Level::Normal => (CLEARED, 10),
        };
        let at = format!("{} {}", number(c.value), c.unit);
        let line = match (c.level, c.threshold) {
            (Level::Normal, _) => format!("{} is back to {at}, below its thresholds", c.name),
            (level, Some(t)) => format!(
                "{} at {at}, at or above its {} threshold of {} {} for {} s",
                c.name,
                if level == Level::Critical { "critical" } else { "high" },
                number(t),
                c.unit,
                self.config.for_seconds
            ),
            (_, None) => format!("{} at {at}", c.name),
        };
        let detail = json!({
            "schema": SCHEMA, "line": line, "sensor": c.sensor, "value": c.value, "unit": c.unit,
            "threshold": c.threshold, "level": c.level, "sequence": self.sequence,
        });
        u.report(occurrence, severity, &line, detail).await
    }

    /// What the kernel signalled, reported as it came.
    pub async fn signal(&mut self, u: &impl Acts, s: Signal) -> Result<(), yoke_sdk::base::Error> {
        match s {
            Signal::Alarm { sensor, attribute, raised } => {
                let name = self.names.get(&sensor).cloned().unwrap_or_else(|| sensor.clone());
                let line = format!("{name}: the kernel {} {attribute}", if raised { "raised" } else { "cleared" });
                let detail = json!({ "schema": SCHEMA, "line": line, "sensor": sensor, "attribute": attribute, "raised": raised });
                u.report(ALARM, if raised { 60 } else { 10 }, &line, detail).await
            }
            Signal::Trip { zone, device, trip, kind, at, temperature, up } => {
                let n = zone.trim_start_matches("thermal_zone");
                let point = match at {
                    Some(t) => format!("its {kind} trip point at {} °C", number(t)),
                    None => format!("its {kind} trip point {trip}"),
                };
                let now = temperature.map(|t| format!(", at {} °C", number(t))).unwrap_or_default();
                let line = if up { format!("{device} zone {n} rose past {point}{now}") } else { format!("{device} zone {n} fell back below {point}{now}") };
                let severity = match (up, kind.as_str()) {
                    (false, _) => 10,
                    (true, "critical") => 80,
                    (true, "hot") => 60,
                    (true, _) => 30,
                };
                let detail = json!({
                    "schema": SCHEMA, "line": line, "zone": zone, "device": device, "trip": trip, "type": kind,
                    "at": at, "temperature": temperature, "direction": if up { "up" } else { "down" },
                });
                u.report(TRIP, severity, &line, detail).await
            }
        }
    }

    pub fn answer(&self, query: &str, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let v = match query {
            SNAPSHOT => {
                let _: NoInput = input(query, payload)?;
                let mut v = self.record();
                v["interval_ms"] = json!(self.config.interval_ms);
                v["sources"] = json!(self.sources);
                v["signals"] = json!(self.backend.listening());
                if self.sequence == 0 {
                    v["note"] = json!("the sensors have not been read yet");
                } else if self.readings.is_empty() {
                    v["note"] = json!(
                        "this host shows no sensors: no hwmon chip, thermal zone, cooling device or IIO channel under /sys, as in a container or a virtual machine"
                    );
                }
                v
            }
            HISTORY => {
                let i: HistoryInput = input(query, payload)?;
                let Some((unit, h)) = self.history.get(&i.id) else {
                    return Err(Error::NotFound(format!("no sensor {} has a value: {SNAPSHOT} names those there are", i.id)));
                };
                let skip = h.len().saturating_sub(i.n.unwrap_or(HISTORY_CAP));
                let points: Vec<Value> = h.iter().skip(skip).map(|(at, v)| json!({ "at_ms": at, "value": v })).collect();
                json!({ "schema": SCHEMA, "id": i.id, "unit": unit, "points": points })
            }
            other => return Err(Error::Invalid(format!("{other} is not a question of {PLUGIN}"))),
        };
        Ok(v.to_string().into_bytes())
    }

    /// Does what one event of the Session asks: a declared question is answered, always; an activation or
    /// a stop of the stream is followed. Nothing else is declared, so nothing else is granted.
    pub async fn handle(&mut self, u: &impl Acts, event: &Event) -> Result<(), yoke_sdk::base::Error> {
        match event {
            Event::Question(q) if QUERIES.contains(&q.r#type.as_str()) => match self.answer(&q.r#type, &q.payload) {
                Ok(answer) => u.answer(q, answer).await,
                Err(e) => u.fail(q, code(&e), &message(&e)).await,
            },
            Event::Activated(a) if a.stream == READINGS => {
                self.flows = true;
                Ok(())
            }
            Event::Stopped { stream } if stream == READINGS => {
                self.flows = false;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn flows(&self) -> bool {
        self.flows
    }

    /// What the health should say, where it changed: how many sensors, from where, how many cannot be read,
    /// and what of the kernel's signals is followed. A host with no sensor is well, and says so.
    pub fn health(&mut self) -> Option<(u8, String)> {
        let total = self.readings.len();
        let failing = self.readings.iter().filter(|r| r["value"].is_null()).count();
        let from: Vec<String> = self.sources.iter().filter(|(_, n)| **n > 0).map(|(s, n)| format!("{n} {s}")).collect();
        let l = self.backend.listening();
        let mut line = if total == 0 {
            "no sensors on this host".to_string()
        } else {
            format!("{total} sensors ({}), {failing} unreadable, read every {} ms", from.join(", "), self.config.interval_ms)
        };
        line += &format!("; {} alarm attributes waited on", l.alarm_attributes);
        line += &match (&l.thermal_events, &l.thermal_why) {
            (true, _) => "; thermal events followed".to_string(),
            (false, Some(why)) => format!("; thermal events not followed: {why}"),
            (false, None) => "; thermal events not followed".to_string(),
        };
        let grade = if total > 0 && failing == total { 50 } else { 0 };
        let now = Some((grade, line));
        if now == self.said {
            return None;
        }
        self.said = now.clone();
        now
    }
}
