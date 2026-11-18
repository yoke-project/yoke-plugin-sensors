//! The reader against a sysfs that is not the host's: a tree of files written for the test, and the kernel's
//! thermal events as the bytes it would send.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sensors::backend::*;
use sensors::netlink;

fn w(p: &Path, content: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sensors-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A laptop's sysfs: an NVMe drive and a CPU, two chips of the same name, a thermal zone with its hwmon
/// mirror, a battery's hwmon, cooling devices, and an IIO light and humidity sensor.
fn laptop(root: &Path) {
    let dev = root.join("devices");
    let hw = root.join("class/hwmon");
    fs::create_dir_all(&hw).unwrap();
    let chip = |n: &str, at: &str| {
        let d = dev.join(at).join(n);
        fs::create_dir_all(&d).unwrap();
        symlink(&d, hw.join(n)).unwrap();
        d
    };
    let nvme = chip("hwmon3", "pci0000:00/nvme/nvme0");
    w(&nvme.join("name"), "nvme\n");
    w(&nvme.join("temp1_input"), "41850\n");
    w(&nvme.join("temp1_label"), "Composite\n");
    w(&nvme.join("temp1_max"), "84850\n");
    w(&nvme.join("temp1_crit"), "89850\n");
    w(&nvme.join("temp1_min"), "-273150\n");
    w(&nvme.join("temp1_alarm"), "0\n");
    w(&nvme.join("temp2_input"), "30000\n");
    w(&nvme.join("temp2_max"), "65261850\n");
    // A directory where a file is expected fails to read, as EIO would.
    fs::create_dir_all(nvme.join("temp3_input")).unwrap();
    let cpu = chip("hwmon5", "pci0000:00/0000:00:18.3");
    w(&cpu.join("name"), "k10temp\n");
    w(&cpu.join("temp1_input"), "55125\n");
    w(&cpu.join("temp1_label"), "Tctl\n");
    let sio = chip("hwmon9", "platform/nct6775.656");
    w(&sio.join("name"), "nct6775\n");
    w(&sio.join("fan1_input"), "1200\n");
    w(&sio.join("fan1_alarm"), "1\n");
    w(&sio.join("in0_input"), "1100\n");
    w(&sio.join("in0_max"), "1300\n");
    w(&sio.join("in0_min_alarm"), "0\n");
    w(&sio.join("pwm1"), "255\n");
    w(&sio.join("pwm1_enable"), "1\n");
    w(&sio.join("power1_input"), "15000000\n");
    w(&sio.join("beep_enable"), "0\n");
    let sio2 = chip("hwmon10", "platform/nct6775.657");
    w(&sio2.join("name"), "nct6775\n");
    w(&sio2.join("fan2_input"), "900\n");
    // A thermal zone's mirror, and a battery's: neither is read as hwmon.
    let mirror = chip("hwmon1", "virtual/thermal/thermal_zone0");
    w(&mirror.join("name"), "acpitz\n");
    w(&mirror.join("temp1_input"), "50000\n");
    let battery = chip("hwmon2", "LNXSYSTM:00/PNP0C0A:00/power_supply/BAT0");
    w(&battery.join("name"), "BAT0\n");
    w(&battery.join("in0_input"), "12300\n");
    // An empty chip gives nothing.
    let empty = chip("hwmon6", "platform/hp-wmi");
    w(&empty.join("name"), "hp\n");
    w(&empty.join("pwm1_enable"), "2\n");

    let th = root.join("class/thermal");
    w(&th.join("thermal_zone0/type"), "acpitz\n");
    w(&th.join("thermal_zone0/temp"), "50000\n");
    w(&th.join("thermal_zone0/trip_point_0_type"), "critical\n");
    w(&th.join("thermal_zone0/trip_point_0_temp"), "128000\n");
    w(&th.join("thermal_zone0/trip_point_1_type"), "hot\n");
    w(&th.join("thermal_zone0/trip_point_1_temp"), "113000\n");
    w(&th.join("thermal_zone0/trip_point_2_type"), "passive\n");
    w(&th.join("thermal_zone0/trip_point_2_temp"), "90000\n");
    w(&th.join("thermal_zone10/type"), "x86_pkg_temp\n");
    w(&th.join("thermal_zone10/temp"), "47000\n");
    w(&th.join("thermal_zone2/type"), "pch\n");
    w(&th.join("thermal_zone2/temp"), "35500\n");
    w(&th.join("cooling_device0/type"), "Processor\n");
    w(&th.join("cooling_device0/cur_state"), "1\n");
    w(&th.join("cooling_device0/max_state"), "4\n");
    w(&th.join("cooling_device1/type"), "Fan\n");
    w(&th.join("cooling_device1/cur_state"), "0\n");
    w(&th.join("cooling_device1/max_state"), "0\n");

    let iio = root.join("bus/iio/devices/iio:device0");
    w(&iio.join("name"), "hdc2010\n");
    w(&iio.join("in_temp_raw"), "26000\n");
    w(&iio.join("in_temp_scale"), "2.517700195\n");
    w(&iio.join("in_temp_offset"), "-15887.515151\n");
    w(&iio.join("in_humidityrelative_input"), "45500\n");
    w(&iio.join("in_illuminance_raw"), "120\n");
    w(&iio.join("in_illuminance_scale"), "0.5\n");
    w(&iio.join("in_voltage0_raw"), "1000\n");
    w(&iio.join("in_voltage_scale"), "1.5\n");
    w(&iio.join("in_accel_x_raw"), "12\n");
    w(&iio.join("sampling_frequency"), "1\n");
}

fn find<'a>(s: &'a Survey, id: &str) -> &'a Sensor {
    s.sensors.iter().find(|x| x.id == id).unwrap_or_else(|| panic!("no {id} in {:?}", s.sensors.iter().map(|x| &x.id).collect::<Vec<_>>()))
}

#[test]
fn every_source_is_read_by_its_fixed_places() {
    let root = tmp("tree");
    laptop(&root);
    let s = survey(&root);
    let ids: Vec<&str> = s.sensors.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "nvme/temp1",
            "nvme/temp2",
            "nvme/temp3",
            "k10temp/temp1",
            "nct6775/fan1",
            "nct6775/in0",
            "nct6775/power1",
            "nct6775/pwm1",
            "nct6775@2/fan2",
            "thermal_zone0",
            "thermal_zone2",
            "thermal_zone10",
            "cooling_device0",
            "cooling_device1",
            "iio:device0/in_humidityrelative",
            "iio:device0/in_illuminance",
            "iio:device0/in_temp",
            "iio:device0/in_voltage0",
        ]
    );
    let composite = find(&s, "nvme/temp1");
    assert_eq!((composite.label.as_str(), composite.device.as_str(), composite.source), ("Composite", "nvme", "hwmon"));
    assert_eq!((composite.value, composite.max, composite.crit), (Some(41.85), Some(84.85), Some(89.85)));
    assert_eq!(find(&s, "nvme/temp2").max, None, "65261.85 °C is no limit");
    let broken = find(&s, "nvme/temp3");
    assert!(broken.value.is_none() && broken.error.is_some(), "a failing sensor does not spoil the rest");
    assert_eq!(find(&s, "nct6775/fan1").value, Some(1200.0));
    assert_eq!(find(&s, "nct6775/fan1").max, None, "a fan has no limit read");
    assert_eq!((find(&s, "nct6775/in0").value, find(&s, "nct6775/in0").max), (Some(1.1), Some(1.3)));
    assert_eq!(find(&s, "nct6775/power1").value, Some(15.0));
    assert_eq!(find(&s, "nct6775/pwm1").value, Some(100.0));
    let zone = find(&s, "thermal_zone0");
    assert_eq!((zone.device.as_str(), zone.label.as_str(), zone.kind), ("acpitz", "zone 0", Kind::Temperature));
    assert_eq!((zone.value, zone.max, zone.crit), (Some(50.0), Some(113.0), Some(128.0)), "the hot and critical trips are its limits");
    assert_eq!(find(&s, "cooling_device0").value, Some(25.0));
    assert_eq!(find(&s, "cooling_device1").value, Some(0.0));
    assert_eq!(find(&s, "cooling_device0").kind.unit(), "%");
    let t = find(&s, "iio:device0/in_temp").value.unwrap();
    assert!((t - 25.4602).abs() < 1e-3, "(raw + offset) × scale, in °C: {t}");
    assert_eq!(find(&s, "iio:device0/in_humidityrelative").value, Some(45.5));
    assert_eq!(find(&s, "iio:device0/in_illuminance").value, Some(60.0));
    assert_eq!(find(&s, "iio:device0/in_voltage0").value, Some(1.5), "the type's shared scale");
    assert_eq!(
        s.sources(),
        BTreeMap::from([("cooling", 2), ("hwmon", 9), ("iio", 4), ("thermal", 3)])
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_host_without_sensors_reads_as_an_empty_survey() {
    let root = tmp("empty");
    let s = survey(&root);
    assert!(s.sensors.is_empty());
    assert!(s.sources().values().all(|n| *n == 0));
    assert!(alarm_attributes(&root).is_empty());
    assert!(Sysfs::at(&root).alarms().is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_files_are_named_as_the_kernel_names_them() {
    assert_eq!(classify_hwmon_file("temp12_input"), Some((Kind::Temperature, "temp", 12, "input")));
    assert_eq!(classify_hwmon_file("in0_label"), Some((Kind::Voltage, "in", 0, "label")));
    assert_eq!(classify_hwmon_file("pwm1"), Some((Kind::Pwm, "pwm", 1, "input")));
    assert_eq!(classify_hwmon_file("temp1_crit_alarm").unwrap().3, "crit_alarm");
    assert_eq!(classify_hwmon_file("power"), None);
    assert_eq!(classify_hwmon_file("name"), None);
    assert_eq!(classify_iio_file("in_humidityrelative_input").map(|c| c.0), Some("humidityrelative"));
    assert_eq!(classify_iio_file("in_voltage3_raw").map(|c| (c.0, c.1)), Some(("voltage3", "voltage")));
    assert_eq!(classify_iio_file("in_temp_ambient_raw").map(|c| c.0), Some("temp_ambient"));
    assert_eq!(classify_iio_file("in_temperature_raw"), None);
    assert_eq!(classify_iio_file("in_accel_x_raw"), None);
    assert_eq!(classify_iio_file("in_temp_scale"), None);
    assert!(natural_key("hwmon9") < natural_key("hwmon10"));
}

#[test]
fn an_alarm_attribute_is_read_when_the_kernel_signals_it() {
    let root = tmp("alarms");
    laptop(&root);
    let found: Vec<(String, String)> = alarm_attributes(&root).into_iter().map(|(s, a, _)| (s, a)).collect();
    assert_eq!(
        found,
        [
            ("nvme/temp1".to_string(), "temp1_alarm".to_string()),
            ("nct6775/fan1".into(), "fan1_alarm".into()),
            ("nct6775/in0".into(), "in0_min_alarm".into()),
        ]
    );
    let raised = Arc::new(Mutex::new(BTreeMap::new()));
    let mut watch = AlarmWatch::new(&root, raised.clone());
    assert_eq!(watch.len(), 3);
    assert_eq!(*raised.lock().unwrap(), BTreeMap::from([("nct6775/fan1".to_string(), vec!["fan1_alarm".to_string()])]));
    assert_eq!(watch.woken(0), None, "woken with nothing changed: no signal");

    // What `poll` says, the test says: the driver changed the attribute and notified it.
    fs::write(root.join("devices/pci0000:00/nvme/nvme0/hwmon3/temp1_alarm"), "1\n").unwrap();
    assert_eq!(watch.woken(0), Some(Signal::Alarm { sensor: "nvme/temp1".into(), attribute: "temp1_alarm".into(), raised: true }));
    assert_eq!(raised.lock().unwrap().len(), 2);
    fs::write(root.join("devices/platform/nct6775.656/hwmon9/fan1_alarm"), "0\n").unwrap();
    assert_eq!(watch.woken(1), Some(Signal::Alarm { sensor: "nct6775/fan1".into(), attribute: "fan1_alarm".into(), raised: false }));

    // A chip that goes is forgotten at the next look; one that comes is followed.
    fs::remove_file(root.join("class/hwmon/hwmon3")).unwrap();
    w(&root.join("devices/platform/nct6775.657/hwmon10/fan2_alarm"), "1\n");
    watch.rescan();
    assert_eq!(watch.len(), 3);
    assert_eq!(*raised.lock().unwrap(), BTreeMap::from([("nct6775@2/fan2".to_string(), vec!["fan2_alarm".to_string()])]));
    let _ = fs::remove_dir_all(&root);
}

/// A thermal event as the kernel sends it: the family's number, the command, and the zone, trip and
/// temperature attributes.
fn event(family: u16, cmd: u8, zone: u32, trip: u32, temp: Option<i32>) -> Vec<u8> {
    let (z, t) = (zone.to_ne_bytes(), trip.to_ne_bytes());
    let mut attrs: Vec<(u16, &[u8])> = vec![(netlink::ATTR_TZ_ID, &z), (netlink::ATTR_TZ_TRIP_ID, &t)];
    let temp = temp.map(|t| t.to_ne_bytes());
    if let Some(temp) = &temp {
        attrs.push((netlink::ATTR_TZ_TEMP, temp));
    }
    netlink::message(family, 0, cmd, &attrs)
}

#[test]
fn a_trip_crossed_is_read_from_the_kernels_event() {
    let mut buf = event(30, netlink::EVENT_TRIP_UP, 0, 1, Some(113_500));
    buf.extend(event(30, netlink::EVENT_TRIP_DOWN, 4, 0, None));
    buf.extend(event(30, 12, 0, 0, None)); // a cooling device's state: not a trip
    buf.extend(event(31, netlink::EVENT_TRIP_UP, 0, 0, None)); // another family
    let got = netlink::parse(&buf, 30);
    assert_eq!(
        got,
        [
            netlink::TripEvent { zone: 0, trip: 1, up: true, temperature: Some(113_500) },
            netlink::TripEvent { zone: 4, trip: 0, up: false, temperature: None },
        ]
    );
    let root = tmp("trip");
    laptop(&root);
    assert_eq!(
        trip_signal(&root, &got[0]),
        Signal::Trip {
            zone: "thermal_zone0".into(),
            device: "acpitz".into(),
            trip: 1,
            kind: "hot".into(),
            at: Some(113.0),
            temperature: Some(113.5),
            up: true,
        }
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_family_and_its_group_are_read_from_the_controllers_answer() {
    let grp = |id: u32, name: &str| {
        let name = [name.as_bytes(), &[0]].concat();
        let mut g = Vec::new();
        for (t, v) in [(2u16, id.to_ne_bytes().to_vec()), (1u16, name)] {
            g.extend(((4 + v.len()) as u16).to_ne_bytes());
            g.extend(t.to_ne_bytes());
            g.extend(&v);
            g.resize((g.len() + 3) & !3, 0);
        }
        g
    };
    let nest = |items: Vec<Vec<u8>>| {
        let mut n = Vec::new();
        for (i, v) in items.into_iter().enumerate() {
            n.extend(((4 + v.len()) as u16).to_ne_bytes());
            n.extend((i as u16 + 1).to_ne_bytes());
            n.extend(&v);
            n.resize((n.len() + 3) & !3, 0);
        }
        n
    };
    let groups = nest(vec![grp(7, "sampling"), grp(8, "event")]);
    let family = 27u16.to_ne_bytes();
    let answer = netlink::message(0x10, 0, 1, &[(1, &family), (2, b"thermal\0"), (7, &groups)]);
    assert_eq!(netlink::resolved(&answer, "event"), Ok((27, 8)));
    assert!(netlink::resolved(&answer, "other").unwrap_err().contains("no group other"));
    let mut refused = vec![0u8; 16];
    refused[4..6].copy_from_slice(&2u16.to_ne_bytes());
    refused.extend((-2i32).to_ne_bytes());
    let len = refused.len() as u32;
    refused[..4].copy_from_slice(&len.to_ne_bytes());
    assert!(netlink::resolved(&refused, "event").unwrap_err().starts_with("the kernel has no thermal events"));
}

/// Run on a real host with `--ignored`: what it reads and follows, read-only.
#[test]
#[ignore]
fn this_host() {
    let s = survey(Path::new(SYS));
    for x in &s.sensors {
        println!("{:<32} {:<12} {:<24} {:?} {:?} max={:?} crit={:?}", x.id, x.device, x.label, x.value, x.error, x.max, x.crit);
    }
    println!("{:?}", s.sources());
    println!("alarm attributes: {:?}", alarm_attributes(Path::new(SYS)));
    match netlink::Events::join() {
        Ok(_) => println!("thermal events: joined"),
        Err(e) => println!("thermal events: {e}"),
    }
}
