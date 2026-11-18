//! follow: asks a unit's questions, follows its streams and its events, and prints each as one line.
//!
//!   follow --instance I --channel C --unit U [--ask Q]... [--stream S]... [--seconds N]
//!
//! Each stream is subscribed to first and started where it does not flow; each question is asked once the
//! subscriptions hold. Every line is `answer <question> <payload>`, `frame <stream> <payload>` or
//! `event <type> [<occurrence>] [<severity>] <detail>`. With no --seconds it runs until interrupted; the
//! streams it started are stopped as it leaves.

use std::time::Duration;

use yoke_proto::interface::v1::subscribed::Carries;
use yoke_sdk::interface::{self, Attachment, Filter, Options};

struct Args {
    address: String,
    unit: String,
    asks: Vec<String>,
    streams: Vec<String>,
    seconds: Option<u64>,
}

fn args() -> Result<Args, String> {
    let (mut instance, mut channel, mut unit) = (None, None, None);
    let (mut asks, mut streams, mut seconds) = (vec![], vec![], None);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} takes a value"));
        match flag.as_str() {
            "--instance" => instance = Some(value()?),
            "--channel" => channel = Some(value()?),
            "--unit" => unit = Some(value()?),
            "--ask" => asks.push(value()?),
            "--stream" => streams.push(value()?),
            "--seconds" => seconds = Some(value()?.parse().map_err(|_| "--seconds takes a number")?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (Some(i), Some(c), Some(unit)) = (instance, channel, unit) else {
        return Err("--instance, --channel and --unit are needed".into());
    };
    let path = interface::application(&i, &c, |k| std::env::var(k).ok()).map_err(|e| e.to_string())?;
    Ok(Args { address: path.to_string_lossy().into_owned(), unit, asks, streams, seconds })
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::main]
async fn main() {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("follow: {e}");
            std::process::exit(2);
        }
    };
    let at = match Attachment::attach(&a.address, Options::new()).await {
        Ok(at) => std::sync::Arc::new(at),
        Err(e) => {
            eprintln!("follow: {e}");
            std::process::exit(1);
        }
    };
    let mut started = vec![];
    let filter = Filter { subject_kind: "unit".into(), subject_identity: a.unit.clone(), ..Default::default() };
    let mut events = at.subscribe(filter).await.ok();
    for s in &a.streams {
        match at.subscribe_stream(&a.unit, s).await {
            Ok(mut delivery) => {
                if !delivery.flowing {
                    match at.start_stream(&a.unit, s).await {
                        Ok(_) => started.push(s.clone()),
                        Err(e) => println!("refused {s} {e}"),
                    }
                }
                let s = s.clone();
                tokio::spawn(async move {
                    while let Ok(Some(f)) = delivery.next().await {
                        println!("frame {s} {}", text(&f.payload));
                    }
                    println!("ended {s}");
                });
            }
            Err(e) => println!("refused {s} {e}"),
        }
    }
    for q in &a.asks {
        match at.ask(&a.unit, q, b"").await {
            Ok(answer) => println!("answer {q} {}", text(&answer)),
            Err(e) => println!("refused {q} {e}"),
        }
    }
    let watching = async {
        loop {
            let Some(sub) = events.as_mut() else { return std::future::pending().await };
            match sub.next().await {
                Ok(Some(s)) => {
                    if let Some(Carries::Event(e)) = s.carries {
                        let class = if e.occurrence.is_empty() { String::new() } else { format!(" {}", e.occurrence) };
                        println!("event {}{class} [{}] {}", e.r#type, e.severity, text(&e.detail));
                    }
                }
                _ => return,
            }
        }
    };
    let deadline = async {
        match a.seconds {
            Some(n) => tokio::time::sleep(Duration::from_secs(n)).await,
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    };
    tokio::select! {
        _ = watching => {},
        _ = deadline => {},
    }
    for s in started {
        let _ = at.stop_stream(&a.unit, &s).await;
    }
}
