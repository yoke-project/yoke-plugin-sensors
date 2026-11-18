# sensors

A Yoke Plugin that reads the host's hardware sensors from sysfs — hwmon chips, the thermal zones and their
cooling devices, and IIO channels: temperatures, fans, voltages, currents, power, humidity, pressure and
light — and says when a threshold has held or the kernel raises an alarm.

| | |
| --- | --- |
| questions | `sensors.snapshot` (the latest reading, with its sequence, where the sensors come from and what is followed), `sensors.history` `{"id": "<sensor>", "n": <count>}` |
| commands | none: the sensors are read, never driven |
| streams | `sensors.readings`: every sensor, whole, at every reading, while it flows |
| occurrences | `sensors.threshold.high` (50), `sensors.threshold.critical` (70), `sensors.threshold.cleared` (10), `sensors.alarm` (60 raised, 10 cleared), `sensors.trip` (80 critical, 60 hot, 30 another trip point, 10 on the way down) |

Every payload is JSON and states `schema`; `sensors.snapshot` takes `{}`. Every occurrence carries its
facts as JSON in its detail — `line`, and the sensor, the value, the threshold, the attribute or the trip
point — which is what an interface client receives.

**The sensors are read at an interval the application sets**, and every reading carries its stream's
`sequence`: the count of readings since the unit started, whether or not the stream flowed. The snapshot
states it. **An interface that attaches late** subscribes to `sensors.readings`, then asks
`sensors.snapshot`, and drops what the stream brings at or below the sequence it was answered with.

**A threshold is the sensor's own limit** — an hwmon chip's `max` and `crit`, a thermal zone's `hot` and
`critical` trip points — or the one the application sets. A reading at or above it is `high` or
`critical`, and is back to `normal` only once it has fallen below it by the hysteresis; the occurrence is
reported once the level has held for `--for-seconds`, and cleared once, when it is normal again.

**What the kernel signals is not read again to be found**: an hwmon alarm attribute is waited on with
`poll` and reported as `sensors.alarm` when its driver notifies it, and a thermal zone crossing a trip
point is `sensors.trip`, from the kernel's thermal events. An alarm whose driver does not notify is not
seen; the snapshot says how many attributes are waited on, and whether the thermal events are followed.

The application sets among the unit's arguments `--interval-ms` (2000), `--for-seconds` (10),
`--hysteresis-celsius` (2), `--high-celsius` and `--critical-celsius` (for every temperature that states no
limit of its own), and `--high <id>=<value>` and `--critical <id>=<value>` for one sensor, over its own.

The sensors are read by their fixed places under `/sys`. A host with few or none — a container, a virtual
machine — runs as well: its snapshot lists what there is, and says so where there is nothing. A
battery's and a supply's hwmon readings are left to what reads the supply.

Nothing is granted by declaring it: an operator grants `query.*.answer`, `stream.sensors.readings.publish`
and each `event.*.report`.

Take it into an application with `yoke-compose add plugin https://github.com/yoke-project/yoke-plugin-sensors@<tag>`.
Build it with `yoke-compose build`, and try it from `try/`.

Licensed under the Apache License 2.0.
