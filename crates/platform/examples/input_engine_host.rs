//! Isolated acceptance host: same start_engine/compiled matcher/executor as the app.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use quicker_domain::*;
    use quicker_platform::*;
    use std::sync::atomic::Ordering;
    let args: Vec<String> = std::env::args().collect();
    let config_path = args
        .windows(2)
        .find(|a| a[0] == "--config")
        .map(|a| &a[1])
        .ok_or("--config required")?;
    let status_path = args
        .windows(2)
        .find(|a| a[0] == "--status")
        .map(|a| &a[1])
        .ok_or("--status required")?;
    let seconds = args
        .windows(2)
        .find(|a| a[0] == "--seconds")
        .and_then(|a| a[1].parse::<u64>().ok())
        .unwrap_or(180);
    let config: Config = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let compiled = compile_config(&config).map_err(|e| format!("Config invalid: {e:?}"))?;
    let shared = Shared::new();
    *shared.config.lock().unwrap() = config;
    *shared.compiled.lock().unwrap() = compiled;
    start_engine(shared.clone())?;
    shared.paused.store(false, Ordering::SeqCst);
    let started = std::time::Instant::now();
    while started.elapsed().as_secs() < seconds {
        let value = serde_json::json!({"input":input_status(),"last":shared.last.lock().unwrap().clone(), "paused":shared.paused.load(Ordering::SeqCst), "generation":shared.generation.load(Ordering::SeqCst)});
        std::fs::write(status_path, serde_json::to_string_pretty(&value)?)?;
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    shared.shutdown.store(true, Ordering::SeqCst);
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows required");
}
