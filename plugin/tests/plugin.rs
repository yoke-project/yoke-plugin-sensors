//! The Plugin against sensors that are not there: a fake backend stands in for sysfs and the kernel, and a
//! recorder for the Core.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sensors::backend::{Kind, Listening, Sensor, SensorBackend, Signal, Survey};
use sensors::plugin::*;
use serde_json::Value;
use tokio::sync::broadcast;
use yoke_sdk::plugin::{Activated, Event, Question};

fn sensor(id: &str, kind: Kind, value: Option<f64>, max: Option<f64>, crit: Option<f64>) -> Sensor {
    Sensor {
        id: id.into(),
        source: "hwmon",
        device: id.split('/').next().unwrap().into(),
        kind,
        label: id.rsplit('/').next().unwrap().into(),
        value,
        error: value.is_none().then(|| "EIO (input/output error)".into()),
        max,
        crit,
        path: PathBuf::from(format!("/sys/class/hwmon/{id}")),
    }
}

/// A laptop: a CPU temperature with limits, an NVMe one with none, a fan, and the alarms the kernel says.
struct Fake {
    raised: Mutex<BTreeMap<String, Vec<String>>>,
    signals: broadcast::Sender<Signal>,
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake { raised: Mutex::default(), signals: broadcast::channel(4).0 })
    }
}

impl SensorBackend for Fake {
    fn survey(&self) -> Survey {
        unreachable!("the tests hand the Plugin its readings")
    }
    fn alarms(&self) -> BTreeMap<String, Vec<String>> {
        self.raised.lock().unwrap().clone()
    }
    fn signals(&self) -> broadcast::Receiver<Signal> {
        self.signals.subscribe()
    }
    fn listening(&self) -> Listening {
        Listening { alarm_attributes: 1, thermal_events: false, thermal_why: Some("the fake has none".into()) }
    }
}

fn laptop(cpu: f64) -> Survey {
    Survey {
        sensors: vec![
            sensor("k10temp/temp1", Kind::Temperature, Some(cpu), Some(80.0), Some(95.0)),
            sensor("nvme/temp1", Kind::Temperature, Some(40.0), None, None),
            sensor("thinkpad/fan1", Kind::Fan, Some(2100.0), Some(1000.0), None),
        ],
    }
}

#[derive(Default)]
struct Core {
    said: Mutex<Vec<String>>,
}

impl Core {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.said.lock().unwrap())
    }
    fn push(&self, line: String) {
        self.said.lock().unwrap().push(line);
    }
}

impl Acts for Core {
    async fn answer(&self, _q: &Question, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("answer {}", String::from_utf8_lossy(&payload)));
        Ok(())
    }
    async fn fail(&self, _q: &Question, code: &str, message: &str) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("fail {code}: {message}"));
        Ok(())
    }
    async fn report(&self, occurrence: &str, severity: u8, line: &str, detail: Value) -> Result<(), yoke_sdk::base::Error> {
        assert_eq!(detail["line"], line, "the detail carries the line a client reads");
        assert_eq!(detail["schema"], SCHEMA);
        self.push(format!("report {occurrence} {severity}: {line}"));
        Ok(())
    }
    async fn emit(&self, stream: &str, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("emit {stream} {}", String::from_utf8_lossy(&payload)));
        Ok(())
    }
}

fn question(r#type: &str, payload: &str) -> Event {
    Event::Question(Question { id: "q-1".into(), r#type: r#type.into(), payload: payload.as_bytes().to_vec() })
}

fn activated(stream: &str) -> Event {
    Event::Activated(Activated { stream: stream.into(), transport: "shared".into(), address: String::new() })
}

fn json_of(line: &str, skip: usize) -> Value {
    serde_json::from_str(line.splitn(skip + 1, ' ').nth(skip).unwrap()).unwrap()
}

/// Read every second, a level reported once it has held three seconds.
fn config() -> Config {
    Config::from_args(["--interval-ms", "1000", "--for-seconds", "3"].map(String::from))
}

async fn snapshot(p: &mut Sensors, core: &Core) -> Value {
    p.handle(core, &question(SNAPSHOT, "")).await.unwrap();
    json_of(&core.take()[0], 1)
}

#[test]
fn the_manifest_kept_is_the_manifest_declared() {
    assert_eq!(declaration().manifest(), include_str!("../com.yoke.sensors.yaml"));
}

#[test]
fn every_object_declared_has_one_capability() {
    let d = declaration();
    assert_eq!(d.capabilities.len(), d.queries.len() + d.commands.len() + d.streams.len() + d.occurrences.len());
    assert!(d.commands.is_empty(), "the sensors are read, never driven");
}

#[test]
fn the_unit_arguments_set_the_rate_and_the_thresholds() {
    let c = Config::from_args(
        [
            "--interval-ms", "500", "--for-seconds", "4", "--hysteresis-celsius", "1.5", "--high-celsius", "70",
            "--critical-celsius", "90", "--high", "nvme/temp1=60", "--critical", "nvme/temp1=75", "--interval-ms", "5",
        ]
        .map(String::from),
    );
    assert_eq!(c.interval_ms, 500, "below 100 ms is not taken");
    assert_eq!(c.readings(), 8);
    assert_eq!(c.hysteresis_celsius, 1.5);
    assert_eq!((c.high_celsius, c.critical_celsius), (Some(70.0), Some(90.0)));
    let s = laptop(50.0).sensors;
    assert_eq!(c.limits(&s[0]), (Some(80.0), Some(95.0)), "a sensor's own limits stand");
    assert_eq!(c.limits(&s[1]), (Some(60.0), Some(75.0)), "the application's for one sensor come first");
    assert_eq!(c.limits(&s[2]), (None, None), "a fan has no threshold: more is not worse");
    let none = sensor("acpi/temp9", Kind::Temperature, Some(1.0), None, None);
    assert_eq!(c.limits(&none), (Some(70.0), Some(90.0)), "the application's for every temperature fill what is missing");
    assert_eq!(Config::from_args(Vec::<String>::new()), Config::default());
}

#[test]
fn a_value_at_a_threshold_does_not_flap() {
    let h = |_| 2.0;
    let (hi, cr) = (Some(80.0), Some(95.0));
    let mut level = Level::Normal;
    let mut changes = 0;
    for v in [79.0, 80.0, 79.0, 80.5, 79.5, 78.5, 79.9, 80.0] {
        let next = evaluate(level, v, hi, cr, h);
        changes += (next != level) as u32;
        level = next;
    }
    assert_eq!((level, changes), (Level::High, 1));
    assert_eq!(evaluate(level, 77.9, hi, cr, h), Level::Normal);
    assert_eq!(evaluate(Level::Normal, 96.0, hi, cr, h), Level::Critical);
    assert_eq!(evaluate(Level::Critical, 94.0, hi, cr, h), Level::Critical);
    assert_eq!(evaluate(Level::Critical, 92.0, hi, cr, h), Level::High);
    assert_eq!(evaluate(Level::Critical, 70.0, hi, cr, h), Level::Normal);
    assert_eq!(evaluate(Level::Normal, 99.0, None, None, h), Level::Normal);
}

#[tokio::test]
async fn the_snapshot_is_the_latest_reading_with_its_sequence() {
    let (fake, core) = (Fake::new(), Core::default());
    fake.raised.lock().unwrap().insert("k10temp/temp1".into(), vec!["temp1_max_alarm".into()]);
    let mut p = Sensors::new(config(), fake);
    let before = snapshot(&mut p, &core).await;
    assert_eq!(before["sequence"], 0);
    assert_eq!(before["note"], "the sensors have not been read yet");

    p.tick(&core, laptop(50.0)).await.unwrap();
    p.tick(&core, laptop(51.0)).await.unwrap();
    assert!(core.take().is_empty(), "nothing flows and nothing was reached");
    let s = snapshot(&mut p, &core).await;
    assert_eq!(s["schema"], SCHEMA);
    assert_eq!(s["sequence"], 2);
    assert_eq!(s["interval_ms"], 1000);
    assert_eq!(s["truncated"], false);
    assert_eq!(s["sources"]["hwmon"], 3);
    assert_eq!(s["sources"]["iio"], 0);
    assert_eq!(s["signals"]["alarm_attributes"], 1);
    assert_eq!(s["signals"]["thermal_events"], false);
    assert!(s.get("note").is_none());
    let cpu = &s["sensors"][0];
    assert_eq!(cpu["id"], "k10temp/temp1");
    assert_eq!(cpu["kind"], "temperature");
    assert_eq!(cpu["unit"], "°C");
    assert_eq!(cpu["value"], 51.0);
    assert_eq!((cpu["high"].clone(), cpu["critical"].clone()), (80.0.into(), 95.0.into()));
    assert_eq!(cpu["level"], "normal");
    assert_eq!(cpu["alarm"][0], "temp1_max_alarm", "the alarms are the kernel's, as last signalled");
    assert_eq!(s["sensors"][2]["level"], Value::Null, "a fan has no level");

    p.handle(&core, &question(SNAPSHOT, r#"{"all":true}"#)).await.unwrap();
    assert!(core.take()[0].starts_with("fail invalid: the payload is not what sensors.snapshot takes"));
}

#[tokio::test]
async fn the_readings_flow_only_while_activated_and_are_counted_whether_or_not() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    p.tick(&core, laptop(50.0)).await.unwrap();
    assert!(core.take().is_empty());
    p.handle(&core, &activated(READINGS)).await.unwrap();
    p.tick(&core, laptop(50.0)).await.unwrap();
    let said = core.take();
    assert_eq!(said.len(), 1);
    let r = json_of(&said[0], 2);
    assert_eq!(r["sequence"], 2, "the first reading was counted though nothing flowed");
    assert_eq!(r["sensors"].as_array().unwrap().len(), 3);
    p.handle(&core, &Event::Stopped { stream: READINGS.into() }).await.unwrap();
    p.tick(&core, laptop(50.0)).await.unwrap();
    assert!(core.take().is_empty());
    assert!(!p.flows());
}

#[tokio::test]
async fn a_late_interface_joins_the_snapshot_and_the_stream_without_a_gap() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    for _ in 0..3 {
        p.tick(&core, laptop(50.0)).await.unwrap();
    }
    // The interface subscribes and starts the stream; two readings flow before its question is answered.
    p.handle(&core, &activated(READINGS)).await.unwrap();
    p.tick(&core, laptop(50.0)).await.unwrap();
    p.tick(&core, laptop(50.0)).await.unwrap();
    p.handle(&core, &question(SNAPSHOT, "{}")).await.unwrap();
    p.tick(&core, laptop(50.0)).await.unwrap();
    p.tick(&core, laptop(50.0)).await.unwrap();
    let said = core.take();
    let at = json_of(&said[2], 1)["sequence"].as_u64().unwrap();
    assert_eq!(at, 5);
    let frames: Vec<u64> = said.iter().filter(|l| l.starts_with("emit")).map(|l| json_of(l, 2)["sequence"].as_u64().unwrap()).collect();
    assert_eq!(frames, [4, 5, 6, 7]);
    let mut seen = vec![at];
    seen.extend(frames.into_iter().filter(|&n| n > at));
    assert!(seen.windows(2).all(|w| w[1] == w[0] + 1), "a gap: {seen:?}");
}

#[tokio::test]
async fn a_threshold_is_reported_once_it_has_held_and_cleared_past_the_hysteresis() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    p.tick(&core, laptop(81.0)).await.unwrap();
    p.tick(&core, laptop(82.0)).await.unwrap();
    assert!(core.take().is_empty(), "two seconds are not three");
    p.tick(&core, laptop(81.0)).await.unwrap();
    assert_eq!(
        core.take(),
        [format!("report {HIGH} 50: k10temp temp1 at 81.0 °C, at or above its high threshold of 80.0 °C for 3 s")]
    );
    p.tick(&core, laptop(79.0)).await.unwrap();
    assert!(core.take().is_empty(), "79 is within the hysteresis: still high");
    // Critical for a moment only: high is what held.
    p.tick(&core, laptop(96.0)).await.unwrap();
    p.tick(&core, laptop(85.0)).await.unwrap();
    assert!(core.take().is_empty());
    for _ in 0..3 {
        p.tick(&core, laptop(97.0)).await.unwrap();
    }
    assert_eq!(
        core.take(),
        [format!("report {CRITICAL} 70: k10temp temp1 at 97.0 °C, at or above its critical threshold of 95.0 °C for 3 s")]
    );
    p.tick(&core, laptop(77.0)).await.unwrap();
    assert_eq!(core.take(), [format!("report {CLEARED} 10: k10temp temp1 is back to 77.0 °C, below its thresholds")]);
    p.tick(&core, laptop(60.0)).await.unwrap();
    assert!(core.take().is_empty(), "cleared once");
}

#[tokio::test]
async fn a_level_that_steps_down_is_armed_again() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    for v in [96.0, 96.0, 96.0] {
        p.tick(&core, laptop(v)).await.unwrap();
    }
    assert!(core.take()[0].starts_with(&format!("report {CRITICAL}")));
    for v in [85.0, 85.0, 85.0] {
        p.tick(&core, laptop(v)).await.unwrap();
    }
    assert!(core.take().is_empty(), "high was said with critical");
    for v in [96.0, 96.0, 96.0] {
        p.tick(&core, laptop(v)).await.unwrap();
    }
    assert!(core.take()[0].starts_with(&format!("report {CRITICAL}")), "critical again, after it had stepped down");
}

#[tokio::test]
async fn what_the_kernel_signals_is_reported_as_it_comes() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    p.tick(&core, laptop(50.0)).await.unwrap();
    let alarm = |raised| Signal::Alarm { sensor: "thinkpad/fan1".into(), attribute: "fan1_alarm".into(), raised };
    p.signal(&core, alarm(true)).await.unwrap();
    p.signal(&core, alarm(false)).await.unwrap();
    let trip = |up, temperature| Signal::Trip {
        zone: "thermal_zone0".into(),
        device: "acpitz".into(),
        trip: 1,
        kind: "hot".into(),
        at: Some(113.0),
        temperature: Some(temperature),
        up,
    };
    p.signal(&core, trip(true, 113.5)).await.unwrap();
    p.signal(&core, trip(false, 100.0)).await.unwrap();
    let critical = Signal::Trip {
        zone: "thermal_zone0".into(),
        device: "acpitz".into(),
        trip: 1,
        kind: "critical".into(),
        at: None,
        temperature: None,
        up: true,
    };
    p.signal(&core, critical).await.unwrap();
    assert_eq!(
        core.take(),
        [
            format!("report {ALARM} 60: thinkpad fan1: the kernel raised fan1_alarm"),
            format!("report {ALARM} 10: thinkpad fan1: the kernel cleared fan1_alarm"),
            format!("report {TRIP} 60: acpitz zone 0 rose past its hot trip point at 113.0 °C, at 113.5 °C"),
            format!("report {TRIP} 10: acpitz zone 0 fell back below its hot trip point at 113.0 °C, at 100.0 °C"),
            format!("report {TRIP} 80: acpitz zone 0 rose past its critical trip point 1"),
        ]
    );
}

#[tokio::test]
async fn the_history_is_the_recent_values_of_one_sensor() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    for v in [50.0, 51.0, 52.0, 53.0] {
        p.tick(&core, laptop(v)).await.unwrap();
    }
    let mut failing = laptop(54.0);
    failing.sensors[0] = sensor("k10temp/temp1", Kind::Temperature, None, Some(80.0), Some(95.0));
    p.tick(&core, failing).await.unwrap();
    p.handle(&core, &question(HISTORY, r#"{"id":"k10temp/temp1","n":2}"#)).await.unwrap();
    p.handle(&core, &question(HISTORY, r#"{"id":"k10temp/temp1"}"#)).await.unwrap();
    p.handle(&core, &question(HISTORY, r#"{"id":"gpu/temp1"}"#)).await.unwrap();
    p.handle(&core, &question(HISTORY, "{}")).await.unwrap();
    p.handle(&core, &question(HISTORY, r#"{"id":"#)).await.unwrap();
    let said = core.take();
    let last = json_of(&said[0], 1);
    assert_eq!(last["unit"], "°C");
    let values: Vec<f64> = last["points"].as_array().unwrap().iter().map(|p| p["value"].as_f64().unwrap()).collect();
    assert_eq!(values, [52.0, 53.0], "a failed read adds no value");
    assert_eq!(json_of(&said[1], 1)["points"].as_array().unwrap().len(), 4);
    assert!(said[2].starts_with("fail not_found: no sensor gpu/temp1"), "{said:?}");
    assert!(said[3].starts_with("fail invalid: the payload is not what sensors.history takes: a field is missing"), "{said:?}");
    assert!(said[4].starts_with("fail invalid: the payload is not what sensors.history takes: it is not JSON"), "{said:?}");
}

#[tokio::test]
async fn a_sensor_that_cannot_be_read_says_why_and_a_gone_one_is_forgotten() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    let mut s = laptop(50.0);
    s.sensors[1] = sensor("nvme/temp1", Kind::Temperature, None, None, None);
    p.tick(&core, s).await.unwrap();
    let snap = snapshot(&mut p, &core).await;
    assert_eq!(snap["sensors"][1]["value"], Value::Null);
    assert_eq!(snap["sensors"][1]["error"], "EIO (input/output error)");
    let mut s = laptop(50.0);
    s.sensors.truncate(1);
    p.tick(&core, s).await.unwrap();
    p.handle(&core, &question(HISTORY, r#"{"id":"thinkpad/fan1"}"#)).await.unwrap();
    assert!(core.take()[0].starts_with("fail not_found"));
}

#[tokio::test]
async fn a_host_without_sensors_says_so_and_is_well() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    p.tick(&core, Survey::default()).await.unwrap();
    let s = snapshot(&mut p, &core).await;
    assert_eq!(s["sequence"], 1);
    assert_eq!(s["sensors"], serde_json::json!([]));
    assert!(s["note"].as_str().unwrap().starts_with("this host shows no sensors"));
    let (grade, line) = p.health().unwrap();
    assert_eq!(grade, 0);
    assert!(line.starts_with("no sensors on this host"), "{line}");
    assert_eq!(p.health(), None, "the health is said again only when it changes");
}

#[tokio::test]
async fn the_health_says_the_sensors_and_what_is_followed() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    p.tick(&core, laptop(50.0)).await.unwrap();
    let (grade, line) = p.health().unwrap();
    assert_eq!(grade, 0);
    assert_eq!(
        line,
        "3 sensors (3 hwmon), 0 unreadable, read every 1000 ms; 1 alarm attributes waited on; thermal events not followed: the fake has none"
    );
    let all_failing = Survey { sensors: vec![sensor("k10temp/temp1", Kind::Temperature, None, None, None)] };
    p.tick(&core, all_failing).await.unwrap();
    assert_eq!(p.health().unwrap().0, 50, "every sensor failing is unwell");
}

#[tokio::test]
async fn an_undeclared_question_or_stream_is_left_alone() {
    let (fake, core) = (Fake::new(), Core::default());
    let mut p = Sensors::new(config(), fake);
    p.handle(&core, &question("sensors.set_interval", r#"{"ms":100}"#)).await.unwrap();
    p.handle(&core, &activated("sensors.other")).await.unwrap();
    assert!(core.take().is_empty());
    assert!(!p.flows());
}

#[test]
fn a_reading_too_large_is_cut_and_says_so() {
    let items: Vec<Value> = (0..10).map(|i| serde_json::json!({ "id": format!("s{i}") })).collect();
    let (kept, cut) = fit(&items, 40);
    assert!(cut && kept.len() < 10 && !kept.is_empty());
    assert_eq!(fit(&items, 10_000), (items.clone(), false));
}
