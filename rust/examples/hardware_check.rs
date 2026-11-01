//! Explicit opt-in real hardware round trip. The journal is retained on any failed restoration.
use sog::{
    config::{Config, CursorMode},
    engine::{Backend, Engine, FileJournal, Outcome},
    native::Native,
    storage,
};
use std::{path::PathBuf, time::Duration};
fn describe(result: &Result<Outcome, String>) -> serde_json::Value {
    match result {
        Ok(o) => {
            serde_json::json!({"ok":true,"channels":o.channels,"warnings":o.warnings,"elapsed_ms":o.elapsed_ms})
        }
        Err(e) => serde_json::json!({"ok":false,"error":e}),
    }
}
fn main() -> Result<(), String> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Pass an absolute evidence directory")?,
    );
    if !dir.is_absolute() {
        return Err("Evidence directory must be absolute".into());
    }
    let mut backend = Native::new(false);
    let baseline = backend.scan()?;
    storage::atomic_json(&dir.join("before.json"), &baseline)?;
    let mut engine = Engine::new(backend, FileJournal(dir.join("hardware-recovery.json")))?;
    engine.light(|| true)?;
    let cfg = Config {
        hdr_off: true,
        cursor_mode: CursorMode::Hide,
        ..Config::default()
    };
    let dark = engine.dark(&cfg, || true);
    std::thread::sleep(Duration::from_millis(200));
    // Executed even if dark returned an error. A second pass retries transient readback failures.
    let mut light = engine.light(|| true);
    if engine.has_pending_recovery() {
        std::thread::sleep(Duration::from_millis(200));
        light = engine.light(|| true);
    }
    let after = engine.backend.scan()?;
    let equal = baseline.devices.iter().all(|d| {
        after
            .devices
            .iter()
            .any(|a| a.id == d.id && a.kind == d.kind && a.value == d.value)
    });
    storage::atomic_json(&dir.join("after.json"), &after)?;
    let pass = dark.is_ok() && light.is_ok() && !engine.has_pending_recovery() && equal;
    storage::atomic_json(
        &dir.join("results.json"),
        &serde_json::json!({"real_hardware":true,"dark":describe(&dark),"restore":describe(&light),"baselines_equal":equal,"pending_recovery":engine.has_pending_recovery(),"pass":pass,
        "hdr_boundary":"Only HDR that was enabled at baseline is switched off. An already disabled HDR display does not prove HDR on/off restoration."}),
    )?;
    if pass {
        Ok(())
    } else {
        Err("Hardware check failed; inspect results and retained recovery journal".into())
    }
}
