//! sensors-ui: a panel for the sensors Plugin, attached to a channel of the application.
//!
//!   sensors-ui --instance I --channel C [--unit U]     attached to a channel of a running instance
//!   sensors-ui [--unit U]                            as a managed interface (YOKE_SOCKET)

mod controller;

slint::include_modules!();

fn args() -> Result<(String, String), String> {
    let (mut instance, mut channel, mut unit) = (None, None, "sensors".to_string());
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} takes a value"));
        match flag.as_str() {
            "--instance" => instance = Some(value()?),
            "--channel" => channel = Some(value()?),
            "--unit" => unit = value()?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let getenv = |k: &str| std::env::var(k).ok();
    let path = match (instance, channel) {
        (Some(i), Some(c)) => yoke_sdk::interface::application(&i, &c, getenv).map_err(|e| e.to_string())?,
        (None, None) => yoke_sdk::interface::managed(getenv).map_err(|e| e.to_string())?,
        _ => return Err("--instance and --channel go together".into()),
    };
    Ok((path.to_string_lossy().into_owned(), unit))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (address, unit) = match args() {
        Ok(a) => a,
        Err(err) => {
            eprintln!("sensors-ui: {err}");
            std::process::exit(2);
        }
    };
    // The runtime outlives the event loop: the attachment's work runs on its workers.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let ui = MainWindow::new()?;
    ui.set_attached_to(format!("{unit} on {address}").into());
    let shared = controller::start(rt.handle().clone(), address, unit, &ui);
    ui.run()?;
    // The window closed: what this panel started is stopped, so that nothing flows for nobody.
    rt.block_on(controller::finish(shared));
    Ok(())
}
