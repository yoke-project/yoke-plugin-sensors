//! Bridges the window and the sensors Plugin, through the interface contract alone.
//!
//! - **Reading.** `sensors.readings` is every sensor at every reading, with a sequence: the panel
//!   subscribes first, starts the stream where it does not flow, then asks `sensors.snapshot`, and drops
//!   every frame at or below the sequence it was answered with. Each reading kept replaces the list.
//! - **The events** are the Core's events about the unit: its state, its health, the occurrences it
//!   reports — a threshold held, an alarm the kernel raised, a trip point crossed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;
use tokio::time::{interval, sleep};
use yoke_proto::interface::v1::subscribed::Carries;
use yoke_sdk::interface::{Attachment, Filter, Options};

use crate::{MainWindow, SensorRow};

const READINGS: &str = "sensors.readings";
const SNAPSHOT: &str = "sensors.snapshot";
const MAX_EVENTS: usize = 300;
const EVENTS_REDRAW: Duration = Duration::from_millis(500);
const AGAIN: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Log {
    lines: VecDeque<String>,
    dirty: bool,
}

pub struct Shared {
    unit: String,
    attachment: OnceLock<Arc<Attachment>>,
    started: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct Ctx {
    at: Arc<Attachment>,
    unit: String,
    ui: Weak<MainWindow>,
    log: Arc<Mutex<Log>>,
    t0: Instant,
    shared: Arc<Shared>,
}

fn say(ui: &Weak<MainWindow>, msg: impl Into<String>) {
    let m = msg.into();
    let _ = ui.upgrade_in_event_loop(move |u| u.set_message(m.into()));
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

pub fn start(rt: Handle, address: String, unit: String, ui: &MainWindow) -> Arc<Shared> {
    let shared = Arc::new(Shared { unit: unit.clone(), attachment: OnceLock::new(), started: Mutex::new(Vec::new()) });
    let weak = ui.as_weak();
    let log = Arc::new(Mutex::new(Log::default()));
    {
        let (log, weak) = (log.clone(), weak.clone());
        ui.on_clear_events(move || {
            let mut l = log.lock().unwrap();
            l.lines.clear();
            l.dirty = false;
            let _ = weak.upgrade_in_event_loop(|u| u.set_events(ModelRc::new(VecModel::default())));
        });
    }
    let shared2 = shared.clone();
    rt.spawn(async move {
        let at = match Attachment::attach(&address, Options::new()).await {
            Ok(at) => Arc::new(at),
            Err(e) => {
                say(&weak, format!("Cannot attach to {address}: {e}"));
                return;
            }
        };
        let _ = shared2.attachment.set(at.clone());
        let c = Ctx { at, unit, ui: weak, log, t0: Instant::now(), shared: shared2 };
        say(&c.ui, "Attached");
        tokio::spawn(redraw_events(c.clone()));
        tokio::spawn(events(c.clone()));
        tokio::spawn(follow(c));
    });
    shared
}

/// Once the window has closed: the streams this panel started are stopped.
pub async fn finish(shared: Arc<Shared>) {
    let Some(at) = shared.attachment.get() else { return };
    let started = std::mem::take(&mut *shared.started.lock().unwrap());
    for stream in started {
        let _ = tokio::time::timeout(Duration::from_secs(2), at.stop_stream(&shared.unit, &stream)).await;
    }
}

async fn ask(c: &Ctx, r#type: &str) -> Result<Value, String> {
    let answer = c.at.ask(&c.unit, r#type, b"").await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&answer).map_err(|e| e.to_string())
}

fn log(c: &Ctx, line: String) {
    let mut l = c.log.lock().unwrap();
    l.lines.push_front(format!("+{:>8.3}s  {line}", c.t0.elapsed().as_secs_f32()));
    l.lines.truncate(MAX_EVENTS);
    l.dirty = true;
}

/// Subscribed first, then asked: what the stream brings at or below the snapshot is dropped.
async fn follow(c: Ctx) {
    loop {
        match c.at.subscribe_stream(&c.unit, READINGS).await {
            Ok(mut delivery) => {
                if !delivery.flowing {
                    match c.at.start_stream(&c.unit, READINGS).await {
                        Ok(_) => c.shared.started.lock().unwrap().push(READINGS.to_string()),
                        Err(e) => log(&c, format!("{READINGS} cannot be started: {e}")),
                    }
                }
                let mut seen = 0u64;
                match ask(&c, SNAPSHOT).await {
                    Ok(v) => {
                        seen = v["sequence"].as_u64().unwrap_or(0);
                        on_snapshot(&c, &v);
                        on_reading(&c, &v);
                    }
                    Err(e) => say(&c.ui, format!("{SNAPSHOT} cannot be asked: {e}")),
                }
                loop {
                    match delivery.next().await {
                        Ok(Some(frame)) => {
                            let Ok(v) = serde_json::from_slice::<Value>(&frame.payload) else { continue };
                            let sequence = v["sequence"].as_u64().unwrap_or(0);
                            if sequence > seen {
                                seen = sequence;
                                on_reading(&c, &v);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log(&c, format!("{READINGS}: {e}"));
                            break;
                        }
                    }
                }
                log(&c, format!("{READINGS}: the delivery ended; subscribing again"));
            }
            Err(e) => log(&c, format!("{READINGS} cannot be subscribed to: {e}")),
        }
        sleep(AGAIN).await;
    }
}

/// What the snapshot alone says: where the sensors come from, what is followed, or that there are none.
fn on_snapshot(c: &Ctx, v: &Value) {
    let sources: Vec<String> = v["sources"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, n)| n.as_u64().unwrap_or(0) > 0)
        .map(|(s, n)| format!("{n} {s}"))
        .collect();
    let sig = &v["signals"];
    let mut summary = if sources.is_empty() { "no sensors".to_string() } else { sources.join(", ") };
    summary += &format!(" · read every {} ms · {} alarm attributes waited on", v["interval_ms"], sig["alarm_attributes"]);
    summary += if sig["thermal_events"].as_bool().unwrap_or(false) { " · thermal events followed" } else { " · thermal events not followed" };
    let note = s(&v["note"]);
    log(c, format!("snapshot #{} {summary}", v["sequence"]));
    let _ = c.ui.upgrade_in_event_loop(move |u| {
        u.set_summary(summary.into());
        if !note.is_empty() {
            u.set_message(note.into());
        } else if u.get_message() == "Attached" || u.get_message() == "Starting..." {
            u.set_message("Ready".into());
        }
    });
}

fn shown(v: &Value) -> String {
    match v.as_f64() {
        Some(x) if x.abs() >= 100.0 => format!("{x:.0}"),
        Some(x) => format!("{x:.1}"),
        None => String::new(),
    }
}

/// One reading: the hottest first among those with a level, then the rest as the Plugin listed them.
fn on_reading(c: &Ctx, v: &Value) {
    let mut list = v["sensors"].as_array().cloned().unwrap_or_default();
    let rank = |r: &Value| match r["level"].as_str() {
        Some("critical") => 0,
        Some("high") => 1,
        _ => 2,
    };
    list.sort_by_key(rank);
    let rows: Vec<SensorRow> = list
        .iter()
        .map(|r| {
            let unit = s(&r["unit"]);
            let value = if r["value"].is_null() { format!("— {}", s(&r["error"])) } else { format!("{} {unit}", shown(&r["value"])) };
            let limits: Vec<String> = [("high", &r["high"]), ("critical", &r["critical"])]
                .into_iter()
                .filter(|(_, t)| !t.is_null())
                .map(|(n, t)| format!("{n} {}", shown(t)))
                .collect();
            let alarm: Vec<String> = r["alarm"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
            SensorRow {
                id: s(&r["id"]).into(),
                name: format!("{} {}", s(&r["device"]), s(&r["label"])).into(),
                kind: format!("{} · {}", s(&r["kind"]), s(&r["source"])).into(),
                value: value.into(),
                limits: limits.join(" · ").into(),
                level: s(&r["level"]).into(),
                alarm: alarm.join(", ").into(),
            }
        })
        .collect();
    let line = format!("reading #{} at {}", v["sequence"], v["at_ms"]);
    let _ = c.ui.upgrade_in_event_loop(move |u| {
        u.set_sensors(ModelRc::new(VecModel::from(rows)));
        u.set_reading(line.into());
    });
}

async fn events(c: Ctx) {
    let filter = Filter { subject_kind: "unit".into(), subject_identity: c.unit.clone(), ..Default::default() };
    let mut sub = match c.at.subscribe(filter).await {
        Ok(s) => s,
        Err(e) => {
            log(&c, format!("events cannot be subscribed to: {e}"));
            return;
        }
    };
    loop {
        match sub.next().await {
            Ok(Some(sub)) => match sub.carries {
                Some(Carries::Event(e)) => {
                    let detail: Value = serde_json::from_slice(&e.detail).unwrap_or(Value::Null);
                    let line = detail["line"].as_str().map(str::to_string).unwrap_or_else(|| {
                        let d = detail.to_string();
                        if d.len() > 160 { format!("{}…", &d[..160]) } else { d }
                    });
                    let class = if e.occurrence.is_empty() { String::new() } else { format!(" {}", e.occurrence) };
                    log(&c, format!("event    {} {}{class} [{}] {line}", e.seq, e.r#type, e.severity));
                    // What the person should see at once: a threshold held, an alarm, a trip point.
                    if e.occurrence.starts_with("sensors.") {
                        say(&c.ui, line);
                    }
                }
                Some(Carries::Overflow(_)) => log(&c, "events: the panel fell behind and some were dropped".into()),
                _ => {}
            },
            Ok(None) => break,
            Err(e) => {
                log(&c, format!("events: {e}"));
                break;
            }
        }
    }
}

async fn redraw_events(c: Ctx) {
    let mut tick = interval(EVENTS_REDRAW);
    loop {
        tick.tick().await;
        let rows = {
            let mut l = c.log.lock().unwrap();
            if !l.dirty {
                continue;
            }
            l.dirty = false;
            l.lines.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>()
        };
        let _ = c.ui.upgrade_in_event_loop(move |u| u.set_events(ModelRc::new(VecModel::from(rows))));
    }
}
