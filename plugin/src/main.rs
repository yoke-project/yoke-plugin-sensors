//! sensors: the host's hardware sensors as a Yoke Plugin unit.

use std::sync::Arc;
use std::time::Duration;

use sensors::backend::{SensorBackend, Sysfs, SYS};
use sensors::plugin::{declaration, Config, Sensors};
use tokio::sync::broadcast::error::RecvError;
use yoke_sdk::plugin::{start, Event};

#[tokio::main]
async fn main() {
    // `sensors --manifest` prints the Manifest the declaration generates, which the bundle carries.
    if std::env::args().nth(1).as_deref() == Some("--manifest") {
        print!("{}", declaration().manifest());
        return;
    }
    let unit = match start(&declaration()).await {
        Ok(unit) => unit,
        Err(err) => {
            eprintln!("sensors: {err}");
            std::process::exit(1);
        }
    };
    // The rate and the thresholds are the application's, among the unit's arguments.
    let config = Config::from_args(std::env::args().skip(1));
    let backend: Arc<dyn SensorBackend> = Sysfs::follow(SYS);
    let mut signals = Some(backend.signals());
    let mut sensors = Sensors::new(config, backend.clone());
    // The first tick is at once: the first reading is what the first health says.
    let mut ticker = tokio::time::interval(Duration::from_millis(sensors.config().interval_ms));
    // After a suspension the missed readings are skipped, not caught up in a burst.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("sensors listens for SIGTERM");
    loop {
        tokio::select! {
            event = unit.next() => match event {
                None | Some(Event::Ended(_)) => return,
                Some(event) => {
                    if let Err(err) = sensors.handle(&unit, &event).await {
                        eprintln!("sensors: {err}");
                    }
                }
            },
            _ = ticker.tick() => {
                let b = backend.clone();
                let survey = tokio::task::spawn_blocking(move || b.survey()).await.unwrap_or_default();
                if let Err(err) = sensors.tick(&unit, survey).await {
                    eprintln!("sensors: {err}");
                }
                if let Some((grade, line)) = sensors.health() {
                    if let Err(err) = unit.health(grade, &line).await {
                        eprintln!("sensors: {err}");
                    }
                }
            }
            signal = async { match signals.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => match signal {
                Ok(s) => {
                    if let Err(err) = sensors.signal(&unit, s).await {
                        eprintln!("sensors: {err}");
                    }
                }
                Err(RecvError::Lagged(n)) => eprintln!("sensors: {n} signals of the kernel were dropped"),
                Err(RecvError::Closed) => signals = None,
            },
            _ = term.recv() => {
                // Asked to stop: close the Session in order, and leave once it has ended.
                let _ = unit.close().await;
                while let Some(event) = unit.next().await {
                    if matches!(event, Event::Ended(_)) {
                        break;
                    }
                }
                return;
            }
        }
    }
}
