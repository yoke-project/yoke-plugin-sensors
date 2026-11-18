//! The host's hardware sensors, read from sysfs by fixed places.
//!
//! - **hwmon** (`class/hwmon`): temperatures, fans, voltages, currents, power, energy, humidity, PWM duty
//!   and frequencies, with the limits each chip states. A chip that mirrors a thermal zone is read as the
//!   zone, and one that belongs to a power supply is left to what reads the supply.
//! - **thermal** (`class/thermal`): each zone's temperature, its `hot` and `critical` trip points as its
//!   limits, and each cooling device's state as a share of its highest.
//! - **IIO** (`bus/iio/devices`): the temperature, voltage, current, power, humidity, pressure and light
//!   channels, scaled as the kernel says.
//!
//! [`Sysfs`] reads every sensor when asked, and follows what the kernel signals by itself: an hwmon alarm
//! attribute its driver notifies, waited on with `poll`; a zone crossing a trip point, from the kernel's
//! thermal events. Nothing is read again to find an alarm. A host with no sensor — a container, a virtual
//! machine — reads as an empty survey, and that is not a failure.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::netlink;

/// Where the kernel shows its devices. A unit gets nothing from the host to say otherwise.
pub const SYS: &str = "/sys";

/// How often the alarm attributes are looked for again, so that a chip that appears is followed too.
const RESCAN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Temperature,
    Fan,
    Voltage,
    Current,
    Power,
    Energy,
    Humidity,
    Pwm,
    Frequency,
    Cooling,
    Pressure,
    Illuminance,
}

impl Kind {
    pub fn unit(self) -> &'static str {
        match self {
            Kind::Temperature => "°C",
            Kind::Fan => "RPM",
            Kind::Voltage => "V",
            Kind::Current => "A",
            Kind::Power => "W",
            Kind::Energy => "J",
            Kind::Humidity => "%RH",
            Kind::Pwm | Kind::Cooling => "%",
            Kind::Frequency => "MHz",
            Kind::Pressure => "kPa",
            Kind::Illuminance => "lx",
        }
    }

    /// Whether a value above its limit is too much: what a threshold means for it.
    pub fn has_limits(self) -> bool {
        matches!(self, Kind::Temperature | Kind::Voltage | Kind::Current | Kind::Power | Kind::Humidity)
    }
}

/// One sensor as last read.
#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    /// Stable while the hardware is: `nvme/temp1` (a second chip of the same name is `nvme@2`),
    /// `thermal_zone0`, `cooling_device3`, `iio:device0/in_temp`.
    pub id: String,
    /// `hwmon`, `thermal`, `cooling` or `iio`.
    pub source: &'static str,
    /// The chip's, zone's or device's name.
    pub device: String,
    pub kind: Kind,
    pub label: String,
    /// In the kind's unit; none where the read failed, and `error` says why.
    pub value: Option<f64>,
    pub error: Option<String>,
    /// The limits the hardware states: above `max` is high, above `crit` critical.
    pub max: Option<f64>,
    pub crit: Option<f64>,
    /// The file it is read from.
    pub path: PathBuf,
}

/// Every sensor, read once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Survey {
    pub sensors: Vec<Sensor>,
}

impl Survey {
    /// How many sensors each source gave.
    pub fn sources(&self) -> BTreeMap<&'static str, usize> {
        let mut m: BTreeMap<&'static str, usize> = ["hwmon", "thermal", "cooling", "iio"].iter().map(|s| (*s, 0)).collect();
        for s in &self.sensors {
            *m.entry(s.source).or_default() += 1;
        }
        m
    }
}

/// What the kernel signalled by itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// An hwmon alarm attribute changed: `temp1_crit_alarm`, `fan2_alarm`, `in0_min_alarm`, `temp3_fault`.
    Alarm { sensor: String, attribute: String, raised: bool },
    /// A thermal zone crossed a trip point.
    Trip {
        /// The zone's sensor id, `thermal_zone0`.
        zone: String,
        device: String,
        trip: u32,
        /// The trip point's type: `critical`, `hot`, `passive`, `active`.
        kind: String,
        /// The trip point's temperature, °C.
        at: Option<f64>,
        /// The zone's temperature as the kernel gave it, °C.
        temperature: Option<f64>,
        up: bool,
    },
}

/// What the unit follows of what the kernel signals.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Listening {
    /// How many hwmon alarm attributes are waited on.
    pub alarm_attributes: usize,
    /// Whether the kernel's thermal events are followed.
    pub thermal_events: bool,
    /// Why they are not, where they are not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal_why: Option<String>,
}

pub trait SensorBackend: Send + Sync + 'static {
    /// Reads every sensor once. Never fails as a whole: a sensor that cannot be read says why.
    fn survey(&self) -> Survey;
    /// The alarm attributes raised now, by sensor, as the kernel last signalled them.
    fn alarms(&self) -> BTreeMap<String, Vec<String>>;
    /// What the kernel signals, as it comes.
    fn signals(&self) -> broadcast::Receiver<Signal>;
    fn listening(&self) -> Listening;
}

// ---------- reading ----------

/// A sysfs integer: `"  42000\n"`.
pub fn parse_int(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

/// An hwmon file's name taken apart: `temp1_input` is (temperature, `temp`, 1, `input`); `pwm1`, which has
/// no suffix, is read as its input.
pub fn classify_hwmon_file(name: &str) -> Option<(Kind, &'static str, u32, &str)> {
    const PREFIXES: [(&str, Kind); 9] = [
        ("temp", Kind::Temperature),
        ("fan", Kind::Fan),
        ("in", Kind::Voltage),
        ("curr", Kind::Current),
        ("power", Kind::Power),
        ("energy", Kind::Energy),
        ("humidity", Kind::Humidity),
        ("pwm", Kind::Pwm),
        ("freq", Kind::Frequency),
    ];
    for (p, kind) in PREFIXES {
        let Some(rest) = name.strip_prefix(p) else { continue };
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            continue;
        }
        let n: u32 = rest[..digits].parse().ok()?;
        let tail = &rest[digits..];
        if tail.is_empty() {
            return (p == "pwm").then_some((kind, p, n, "input"));
        }
        return tail.strip_prefix('_').map(|suffix| (kind, p, n, suffix));
    }
    None
}

/// From the raw hwmon integer to the kind's unit.
pub fn hwmon_scale(kind: Kind) -> f64 {
    match kind {
        Kind::Temperature | Kind::Voltage | Kind::Current | Kind::Humidity => 1e-3,
        Kind::Power | Kind::Energy | Kind::Frequency => 1e-6,
        Kind::Fan | Kind::Cooling | Kind::Pressure | Kind::Illuminance => 1.0,
        Kind::Pwm => 100.0 / 255.0,
    }
}

/// `hwmon10` sorts after `hwmon9`.
pub fn natural_key(name: &str) -> (String, u64) {
    let digits = name.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    let (stem, n) = name.split_at(name.len() - digits);
    (stem.to_string(), n.parse().unwrap_or(0))
}

fn read_trim(p: &Path) -> Result<String, String> {
    fs::read_to_string(p).map(|s| s.trim().to_string()).map_err(|e| describe_io(&e))
}

fn describe_io(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EIO) => "EIO (input/output error)".into(),
        Some(libc::ENODATA) => "ENODATA (no data)".into(),
        Some(libc::EACCES) => "EACCES (permission denied)".into(),
        Some(libc::ENODEV) => "ENODEV (device gone)".into(),
        Some(libc::EAGAIN) => "EAGAIN (not ready)".into(),
        _ => e.to_string(),
    }
}

fn round(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

fn read_scaled(p: &Path, scale: f64) -> Result<f64, String> {
    let s = read_trim(p)?;
    parse_int(&s).map(|v| round(v as f64 * scale)).ok_or_else(|| format!("not a number: {s:?}"))
}

fn sorted_entries(dir: &Path, prefix: &str) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(prefix).then(|| (n, e.path()))
        })
        .collect();
    v.sort_by_key(|(n, _)| natural_key(n));
    v
}

fn sensor(id: String, source: &'static str, device: &str, kind: Kind, label: String, path: PathBuf, read: Result<f64, String>) -> Sensor {
    let (value, error) = match read {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    Sensor { id, source, device: device.into(), kind, label, value, error, max: None, crit: None, path }
}

/// An hwmon chip read here, with the id its sensors are named under.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub id: String,
    pub name: String,
    pub dir: PathBuf,
}

/// The hwmon chips read here: not a thermal zone's mirror, which is read as the zone, nor a power
/// supply's, which belongs to what reads the supply. A second chip of a name is `name@2`.
pub fn hwmon_chips(root: &Path) -> Vec<Chip> {
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    let mut chips = Vec::new();
    for (dirname, dir) in sorted_entries(&root.join("class/hwmon"), "hwmon") {
        let real = fs::canonicalize(&dir).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
        if real.contains("/virtual/thermal/") || real.contains("/power_supply/") {
            continue;
        }
        let name = read_trim(&dir.join("name")).ok().filter(|n| !n.is_empty() && !n.contains('/')).unwrap_or(dirname);
        let n = seen.entry(name.clone()).or_default();
        *n += 1;
        let id = if *n == 1 { name.clone() } else { format!("{name}@{n}") };
        chips.push(Chip { id, name, dir });
    }
    chips
}

fn hwmon(root: &Path) -> Vec<Sensor> {
    let mut out = Vec::new();
    for chip in hwmon_chips(root) {
        let mut inputs: BTreeMap<(Kind, u32), &'static str> = BTreeMap::new();
        for e in fs::read_dir(&chip.dir).into_iter().flatten().flatten() {
            if let Some((kind, prefix, n, "input")) = classify_hwmon_file(&e.file_name().to_string_lossy()) {
                inputs.insert((kind, n), prefix);
            }
        }
        for ((kind, n), prefix) in inputs {
            let stem = format!("{prefix}{n}");
            let path = if kind == Kind::Pwm { chip.dir.join(&stem) } else { chip.dir.join(format!("{stem}_input")) };
            let scale = hwmon_scale(kind);
            let label = read_trim(&chip.dir.join(format!("{stem}_label"))).ok().filter(|l| !l.is_empty()).unwrap_or_else(|| stem.clone());
            let read = read_scaled(&path, scale);
            let mut s = sensor(format!("{}/{stem}", chip.id), "hwmon", &chip.name, kind, label, path, read);
            if kind.has_limits() {
                let limit = |what: &str| read_scaled(&chip.dir.join(format!("{stem}_{what}")), scale).ok();
                s.max = limit("max");
                s.crit = limit("crit");
            }
            // Some drivers state -273.15 or 65261.85 where they mean no limit.
            if kind == Kind::Temperature {
                let sane = |v: &f64| (-100.0..=200.0).contains(v);
                s.max = s.max.filter(sane);
                s.crit = s.crit.filter(sane);
            }
            out.push(s);
        }
    }
    out
}

/// A zone's trip points, `(type, °C)`, in the kernel's order.
pub fn trips(zone: &Path) -> Vec<(String, f64)> {
    let mut trips = Vec::new();
    for i in 0..64 {
        let Ok(kind) = read_trim(&zone.join(format!("trip_point_{i}_type"))) else { break };
        if let Ok(t) = read_scaled(&zone.join(format!("trip_point_{i}_temp")), 1e-3) {
            trips.push((kind, t));
        }
    }
    trips
}

fn thermal(root: &Path) -> Vec<Sensor> {
    let base = root.join("class/thermal");
    let mut out = Vec::new();
    for (name, dir) in sorted_entries(&base, "thermal_zone") {
        let ty = read_trim(&dir.join("type")).unwrap_or_else(|_| name.clone());
        let n = name.trim_start_matches("thermal_zone");
        let path = dir.join("temp");
        let read = read_scaled(&path, 1e-3);
        let mut s = sensor(name.clone(), "thermal", &ty, Kind::Temperature, format!("zone {n}"), path, read);
        let lowest = |kind: &str| trips(&dir).into_iter().filter(|(k, _)| k == kind).map(|(_, t)| t).reduce(f64::min);
        s.max = lowest("hot");
        s.crit = lowest("critical");
        out.push(s);
    }
    for (name, dir) in sorted_entries(&base, "cooling_device") {
        let ty = read_trim(&dir.join("type")).unwrap_or_else(|_| name.clone());
        let n = name.trim_start_matches("cooling_device");
        let max = read_trim(&dir.join("max_state")).ok().and_then(|s| parse_int(&s)).unwrap_or(0);
        let path = dir.join("cur_state");
        let read = read_scaled(&path, 1.0).map(|c| if max > 0 { round(c / max as f64 * 100.0) } else { 0.0 });
        out.push(sensor(name.clone(), "cooling", &ty, Kind::Cooling, format!("{ty} {n}"), path, read));
    }
    out
}

/// The IIO channel types read here, the kind each is, and what turns the kernel's unit into the kind's.
const IIO_TYPES: [(&str, Kind, f64); 7] = [
    ("temp", Kind::Temperature, 1e-3),
    ("voltage", Kind::Voltage, 1e-3),
    ("current", Kind::Current, 1e-3),
    ("power", Kind::Power, 1e-3),
    ("humidityrelative", Kind::Humidity, 1e-3),
    ("pressure", Kind::Pressure, 1.0),
    ("illuminance", Kind::Illuminance, 1.0),
];

/// An IIO file's name taken apart: `in_voltage0_raw` is (`voltage0`, voltage, `raw`). Only the types above,
/// and only `_input` (already scaled) and `_raw`.
pub fn classify_iio_file(name: &str) -> Option<(&str, &'static str, Kind, f64, &str)> {
    let rest = name.strip_prefix("in_")?;
    let (channel, suffix) = rest.rsplit_once('_')?;
    if suffix != "input" && suffix != "raw" {
        return None;
    }
    // The longest type that starts the channel, so that `humidityrelative` is not read as something shorter.
    let (ty, kind, unit) = IIO_TYPES.iter().filter(|(t, _, _)| channel.starts_with(t)).max_by_key(|(t, _, _)| t.len())?;
    let after = &channel[ty.len()..];
    if !(after.is_empty() || after.starts_with(|c: char| c.is_ascii_digit() || c == '_')) {
        return None;
    }
    Some((channel, ty, *kind, *unit, suffix))
}

fn read_f64(p: &Path) -> Option<f64> {
    read_trim(p).ok()?.parse().ok()
}

fn iio(root: &Path) -> Vec<Sensor> {
    let mut out = Vec::new();
    for (name, dir) in sorted_entries(&root.join("bus/iio/devices"), "iio:device") {
        let device = read_trim(&dir.join("name")).unwrap_or_else(|_| name.clone());
        let mut files: Vec<String> = fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        let mut channels: BTreeMap<String, (&'static str, Kind, f64, bool)> = BTreeMap::new();
        for f in &files {
            if let Some((channel, ty, kind, unit, suffix)) = classify_iio_file(f) {
                let processed = suffix == "input";
                let e = channels.entry(channel.to_string()).or_insert((ty, kind, unit, processed));
                e.3 |= processed;
            }
        }
        for (channel, (ty, kind, unit, processed)) in channels {
            let read = if processed {
                let path = dir.join(format!("in_{channel}_input"));
                let v = read_trim(&path).and_then(|s| s.parse::<f64>().map_err(|_| format!("not a number: {s:?}")));
                (path, v.map(|v| round(v * unit)))
            } else {
                let path = dir.join(format!("in_{channel}_raw"));
                let either = |what: &str| read_f64(&dir.join(format!("in_{channel}_{what}"))).or_else(|| read_f64(&dir.join(format!("in_{ty}_{what}"))));
                let (scale, offset) = (either("scale").unwrap_or(1.0), either("offset").unwrap_or(0.0));
                let v = read_trim(&path).and_then(|s| s.parse::<f64>().map_err(|_| format!("not a number: {s:?}")));
                (path, v.map(|raw| round((raw + offset) * scale * unit)))
            };
            let label = read_trim(&dir.join(format!("in_{channel}_label"))).ok().filter(|l| !l.is_empty()).unwrap_or_else(|| channel.clone());
            out.push(sensor(format!("{name}/in_{channel}"), "iio", &device, kind, label, read.0, read.1));
        }
    }
    out
}

/// Every sensor under `root`, read once.
pub fn survey(root: &Path) -> Survey {
    let mut sensors = hwmon(root);
    sensors.extend(thermal(root));
    sensors.extend(iio(root));
    Survey { sensors }
}

// ---------- the alarms the kernel signals ----------

/// The hwmon alarm attributes under `root`, as `(sensor id, attribute, path)`.
pub fn alarm_attributes(root: &Path) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    for chip in hwmon_chips(root) {
        let mut files: Vec<(String, PathBuf)> = fs::read_dir(&chip.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        files.sort();
        for (f, path) in files {
            if let Some((_, prefix, n, suffix)) = classify_hwmon_file(&f) {
                if suffix.ends_with("alarm") || suffix == "fault" {
                    out.push((format!("{}/{prefix}{n}", chip.id), f.clone(), path));
                }
            }
        }
    }
    out
}

struct Watched {
    sensor: String,
    attribute: String,
    path: PathBuf,
    file: File,
    raised: bool,
}

fn read_flag(file: &mut File) -> std::io::Result<bool> {
    let mut s = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut s)?;
    Ok(s.trim() != "0" && !s.trim().is_empty())
}

/// The alarm attributes, each held open: a read arms the kernel's notification, and `poll` waits for it.
pub struct AlarmWatch {
    root: PathBuf,
    files: Vec<Watched>,
    raised: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
}

impl AlarmWatch {
    pub fn new(root: &Path, raised: Arc<Mutex<BTreeMap<String, Vec<String>>>>) -> AlarmWatch {
        let mut w = AlarmWatch { root: root.to_path_buf(), files: Vec::new(), raised };
        w.rescan();
        w
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Opens the attributes that appeared and forgets those gone. Each opened is read once, which is its
    /// state now, and no signal: what was raised before the unit looked is said by the readings.
    pub fn rescan(&mut self) {
        let found = alarm_attributes(&self.root);
        self.files.retain(|w| found.iter().any(|(_, _, p)| *p == w.path));
        for (sensor, attribute, path) in found {
            if self.files.iter().any(|w| w.path == path) {
                continue;
            }
            let Ok(mut file) = File::open(&path) else { continue };
            let Ok(raised) = read_flag(&mut file) else { continue };
            self.files.push(Watched { sensor, attribute, path, file, raised });
        }
        self.publish();
    }

    fn publish(&self) {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for w in self.files.iter().filter(|w| w.raised) {
            m.entry(w.sensor.clone()).or_default().push(w.attribute.clone());
        }
        *self.raised.lock().unwrap() = m;
    }

    /// The kernel signalled the `i`th attribute: it is read, which also arms the next notification, and
    /// a change is a signal. One that can no longer be read is forgotten.
    pub fn woken(&mut self, i: usize) -> Option<Signal> {
        let w = self.files.get_mut(i)?;
        match read_flag(&mut w.file) {
            Ok(raised) if raised != w.raised => {
                w.raised = raised;
                let signal = Signal::Alarm { sensor: w.sensor.clone(), attribute: w.attribute.clone(), raised };
                self.publish();
                Some(signal)
            }
            Ok(_) => None,
            Err(_) => {
                self.files.remove(i);
                self.publish();
                None
            }
        }
    }

    /// Waits on every attribute, and says each change; looks for new ones every [`RESCAN`].
    fn run(mut self, tx: broadcast::Sender<Signal>, listening: Arc<Mutex<Listening>>) {
        let mut scanned = Instant::now();
        loop {
            listening.lock().unwrap().alarm_attributes = self.files.len();
            if self.files.is_empty() {
                std::thread::sleep(RESCAN);
            } else {
                let mut fds: Vec<libc::pollfd> = self
                    .files
                    .iter()
                    .map(|w| libc::pollfd { fd: w.file.as_raw_fd(), events: libc::POLLPRI | libc::POLLERR, revents: 0 })
                    .collect();
                // SAFETY: the array is valid for its length, and every descriptor is held open by `self`.
                let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, RESCAN.as_millis() as libc::c_int) };
                if n > 0 {
                    // From the last, so that one forgotten does not move those still to be read.
                    for i in (0..fds.len()).rev() {
                        if fds[i].revents & (libc::POLLPRI | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                            if let Some(s) = self.woken(i) {
                                let _ = tx.send(s);
                            }
                        }
                    }
                } else if n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            if scanned.elapsed() >= RESCAN {
                self.rescan();
                scanned = Instant::now();
            }
        }
    }
}

// ---------- the backend ----------

/// The sensors under a sysfs root.
pub struct Sysfs {
    root: PathBuf,
    raised: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    listening: Arc<Mutex<Listening>>,
    tx: broadcast::Sender<Signal>,
}

impl Sysfs {
    /// Reads under `root`, and follows nothing.
    pub fn at(root: impl Into<PathBuf>) -> Arc<Sysfs> {
        Arc::new(Sysfs {
            root: root.into(),
            raised: Arc::default(),
            listening: Arc::new(Mutex::new(Listening { thermal_why: Some("not followed".into()), ..Default::default() })),
            tx: broadcast::channel(256).0,
        })
    }

    /// Reads under `root`, and follows what the kernel signals: the alarm attributes, each on a thread
    /// that waits, and the thermal events, on another.
    pub fn follow(root: impl Into<PathBuf>) -> Arc<Sysfs> {
        let me = Sysfs::at(root);
        let watch = AlarmWatch::new(&me.root, me.raised.clone());
        me.listening.lock().unwrap().alarm_attributes = watch.len();
        let (tx, listening) = (me.tx.clone(), me.listening.clone());
        std::thread::Builder::new().name("alarms".into()).spawn(move || watch.run(tx, listening)).ok();
        let (tx, listening, root) = (me.tx.clone(), me.listening.clone(), me.root.clone());
        match netlink::Events::join() {
            Ok(events) => {
                {
                    let mut l = listening.lock().unwrap();
                    l.thermal_events = true;
                    l.thermal_why = None;
                }
                std::thread::Builder::new().name("thermal".into()).spawn(move || follow_trips(events, &root, tx, listening)).ok();
            }
            Err(why) => listening.lock().unwrap().thermal_why = Some(why),
        }
        me
    }
}

/// A trip event as the plugin reads it: the zone's id and name, and what the trip point is, from the
/// zone's own files.
pub fn trip_signal(root: &Path, e: &netlink::TripEvent) -> Signal {
    let dir = root.join(format!("class/thermal/thermal_zone{}", e.zone));
    let kind = read_trim(&dir.join(format!("trip_point_{}_type", e.trip))).unwrap_or_else(|_| "unknown".into());
    Signal::Trip {
        zone: format!("thermal_zone{}", e.zone),
        device: read_trim(&dir.join("type")).unwrap_or_default(),
        trip: e.trip,
        kind,
        at: read_scaled(&dir.join(format!("trip_point_{}_temp", e.trip)), 1e-3).ok(),
        temperature: e.temperature.map(|t| t as f64 / 1000.0),
        up: e.up,
    }
}

fn follow_trips(events: netlink::Events, root: &Path, tx: broadcast::Sender<Signal>, listening: Arc<Mutex<Listening>>) {
    loop {
        match events.next() {
            Ok(trips) => {
                for e in trips {
                    let _ = tx.send(trip_signal(root, &e));
                }
            }
            Err(why) => {
                let mut l = listening.lock().unwrap();
                l.thermal_events = false;
                l.thermal_why = Some(why);
                return;
            }
        }
    }
}

impl SensorBackend for Sysfs {
    fn survey(&self) -> Survey {
        survey(&self.root)
    }
    fn alarms(&self) -> BTreeMap<String, Vec<String>> {
        self.raised.lock().unwrap().clone()
    }
    fn signals(&self) -> broadcast::Receiver<Signal> {
        self.tx.subscribe()
    }
    fn listening(&self) -> Listening {
        self.listening.lock().unwrap().clone()
    }
}
