# Trying sensors

This directory is an application project that uses the Plugin in `..`: `yoke-compose init` made it and
`add plugin .. --yes` took the Plugin into it. Nothing here is the Plugin, and taking the Plugin into
another application never takes this directory.

| Path | What it is |
| --- | --- |
| `yoke.yaml` | the application: the unit `sensors`, read every two seconds with a level held ten seconds before it is reported, and the channel `panel`, a local socket for one client |
| `ui/` | the panel: an interface client written with Slint and the Rust interface library. It shows every sensor with its latest value, its thresholds, its level and its alarms, and says at once when a threshold has held or the kernel signals. It reaches the Plugin through the contract alone and links none of its code |
| `follow/` | an interface client: it follows the stream and every event of the unit, asks the snapshot, and prints each as one line |
| `run.sh` | runs the application on a local Core, grants what the Plugin declares, and opens the panel, or follows the unit with `--follow` |

It needs Yoke installed for the account, with the script published with every release:

    curl --proto '=https' --tlsv1.2 -sSfL https://github.com/yoke-project/yoke/releases/latest/download/yoke-install.sh | sh -s -- --install account

```sh
./run.sh                        # the panel; closing it stops everything
./run.sh --follow --seconds 10  # one line per answer, frame and event, in place of the panel
```

The panel subscribes to `sensors.readings` and starts it where it does not flow, then asks
`sensors.snapshot` and drops what the stream brings at or below the sequence it was answered with.
Closing the window stops the stream the panel started. A sensor's recent values are asked from another
terminal, as `run.sh --follow` says.

After a change to the Plugin, `run.sh` carries it again. To take it by hand:

    yoke-compose remove units.sensors
    rm components/plugins/sensors manifests/com.yoke.sensors.yaml
    yoke-compose add plugin ..
