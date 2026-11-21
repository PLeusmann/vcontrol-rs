//! `vcontrol-mqtt`: a Bridge between a Viessmann controller and an MQTT broker. See `docs/mqtt-architecture.md`.

use std::{env, fs, path::PathBuf, process::ExitCode};

use clap::{Arg, ArgAction, Command, crate_version, value_parser};
use rumqttc::AsyncClient;
use tokio::signal::unix::{SignalKind, signal};

mod bridge;
mod config;
mod controller;
mod discovery;
mod payload;
mod schedule;
mod topics;

use crate::{
  bridge::{Bridge, Exit},
  config::Config,
  controller::Controller,
};

/// The Optolink link is lost or cannot be opened: systemd restarts the Bridge.
const EXIT_LINK: u8 = 1;
/// The configuration is wrong: restarting would not help.
const EXIT_CONFIG: u8 = 2;

async fn shutdown_signal() {
  let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
  let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");

  tokio::select! {
    _ = terminate.recv() => (),
    _ = interrupt.recv() => (),
  }
}

async fn run() -> Result<Exit, (u8, String)> {
  let matches = Command::new("vcontrol-mqtt")
    .version(crate_version!())
    .arg(
      Arg::new("config")
        .short('c')
        .long("config")
        .required(true)
        .value_parser(value_parser!(PathBuf))
        .help("path of the TOML configuration file"),
    )
    .arg(
      Arg::new("read-only")
        .long("read-only")
        .action(ArgAction::SetTrue)
        .help("validate and log received writes, but never send them to the controller"),
    )
    .get_matches();
  let path = matches.get_one::<PathBuf>("config").expect("required");
  let read_only = matches.get_flag("read-only");

  // Startup order: read the TOML, connect Optolink, detect the Device, check the config, connect MQTT.
  let toml = fs::read_to_string(path).map_err(|err| (EXIT_CONFIG, format!("{}: {err}", path.display())))?;
  let password = env::var("VCONTROL_MQTT_PASSWORD").ok().filter(|password| !password.is_empty());
  let config = Config::parse(&toml, password).map_err(|err| (EXIT_CONFIG, format!("{}: {err}", path.display())))?;

  let controller =
    controller::connect(&config.optolink).await.map_err(|err| (EXIT_LINK, format!("Optolink: {err}")))?;
  log::info!("Connected to '{}'.", controller.device().name());

  let config = config
    .check(controller.device())
    .map_err(|problems| (EXIT_CONFIG, format!("{}:\n  {}", path.display(), problems.join("\n  "))))?;

  let ca = match &config.mqtt().ca_file {
    Some(ca_file) if config.mqtt().tls => {
      Some(fs::read(ca_file).map_err(|err| (EXIT_CONFIG, format!("{}: {err}", ca_file.display())))?)
    },
    _ => None,
  };
  let (client, event_loop) = AsyncClient::new(bridge::mqtt_options(&config, ca), bridge::capacity(&config));

  if read_only {
    log::info!("Read-only mode: writes are validated and logged, but not sent to the controller.");
  }
  Ok(Bridge::new(controller, config, client, read_only).run(event_loop, shutdown_signal()).await)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  match run().await {
    Ok(Exit::Shutdown) => ExitCode::SUCCESS,
    Ok(Exit::LinkLost) => ExitCode::from(EXIT_LINK),
    Err((code, message)) => {
      eprintln!("Error: {message}");
      ExitCode::from(code)
    },
  }
}
