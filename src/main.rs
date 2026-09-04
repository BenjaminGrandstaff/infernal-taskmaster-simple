//! Goal: start the process while keeping all testable behavior in the
//! library crate.

use std::env;
use std::thread;

use infernal_taskmaster_simple::health;

const HEALTH_ADDRESS_ENV: &str = "HEALTH_ADDRESS";
const DEFAULT_HEALTH_ADDRESS: &str = "0.0.0.0:8090";

fn main() {
    let config = match infernal_taskmaster_simple::Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            std::process::exit(1);
        }
    };

    // Started before the work loop so Kubernetes can probe during startup
    // and enrollment rather than seeing a refused connection.
    let heartbeat = health::Heartbeat::new();
    let health_heartbeat = heartbeat.clone();
    let health_address =
        env::var(HEALTH_ADDRESS_ENV).unwrap_or_else(|_| DEFAULT_HEALTH_ADDRESS.to_owned());
    thread::spawn(move || {
        if let Err(error) = health::serve(
            &health_address,
            health_heartbeat,
            health::Thresholds::default(),
        ) {
            eprintln!("health endpoint failed: {error}");
        }
    });

    infernal_taskmaster_simple::run(config, heartbeat);
}
