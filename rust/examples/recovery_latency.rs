//! Explicit one-device recovery check, without overlays, cursor changes, or HDR.
use sog::{
    config::{Config, CursorMode},
    engine::{
        Backend, Change, ChangeResult, Device, Engine, FileJournal, Inventory, Kind,
        PresentationAck, RestoreBatch, ScanRequest,
    },
    native::Native,
    storage,
};
use std::{path::PathBuf, sync::Mutex, time::Instant};

struct Observed(Native);
impl Backend for Observed {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.0.scan()
    }
    fn scan_required(&mut self, request: ScanRequest) -> Result<Inventory, String> {
        self.0.scan_required(request)
    }
    fn prepare_restore(&mut self, entries: &[Device], reuse: bool) -> Result<Vec<String>, String> {
        self.0.prepare_restore(entries, reuse)
    }
    // CURRENT_REBIND_BEGIN
    fn rebind_brightness(&mut self, entries: &[Device]) -> Result<Vec<String>, String> {
        self.0.rebind_brightness(entries)
    }
    // CURRENT_REBIND_END
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        self.0.write(id, kind, value)
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        self.0.read(id, kind)
    }
    fn cursor(&mut self, _: bool) -> Result<(), String> {
        Err("This check must not change the cursor".into())
    }
    fn remote_active(&self) -> bool {
        false
    }
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        self.0.apply(changes, current)
    }
    fn apply_restore(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        _: bool,
    ) -> RestoreBatch {
        let written = Mutex::new(None);
        let results = self.0.apply_with_write_ack(changes, current, &|| {
            *written.lock().unwrap() = Some(Instant::now());
        });
        RestoreBatch {
            changes: results,
            presentation: written
                .into_inner()
                .unwrap()
                .map(|at| PresentationAck { result: Ok(()), at }),
        }
    }
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().ok_or("Pass an absolute evidence directory")?);
    let id = args
        .next()
        .ok_or("Pass exactly one complete ddc: identity")?
        .to_ascii_lowercase();
    if !dir.is_absolute() || !id.starts_with("ddc:") || args.next().is_some() {
        return Err("Absolute directory and one exact ddc: identity required".into());
    }
    let mut engine = Engine::new(
        Observed(Native::restricted_to(vec![id.clone()])?),
        FileJournal(dir.join("recovery.json")),
    )?;
    engine.light(|| true)?;
    if engine.has_pending_recovery() {
        return Err("Prior recovery remains pending".into());
    }
    let request = ScanRequest {
        external: true,
        internal: false,
        hdr: false,
    };
    let before = engine.backend.scan_required(request)?;
    if before.devices.len() != 1 || before.devices[0].id != id {
        return Err("The allowlist did not resolve to exactly the requested device".into());
    }
    storage::atomic_json(&dir.join("before.json"), &before)?;
    let cfg = Config {
        dim_internal: false,
        dim_external: true,
        hdr_off: false,
        cursor_mode: CursorMode::Keep,
        ..Config::default()
    };
    let mut rounds = Vec::new();
    for _ in 0..3 {
        let stats = engine.backend.0.stats();
        let dark = engine.dark(&cfg, || true);
        let light = engine.light(|| true);
        if engine.has_pending_recovery() {
            engine.light(|| true)?;
        }
        let dark = dark?;
        let light = light?;
        let after = engine.backend.scan_required(request)?;
        let exact = after.devices.len() == 1
            && after.devices[0].id == id
            && after.devices[0].value == before.devices[0].value;
        let pass = exact
            && !engine.has_pending_recovery()
            && dark.warnings.is_empty()
            && light.warnings.is_empty()
            && dark.channels == 1
            && light.channels == 1;
        rounds.push(
            serde_json::json!({"pass":pass,"baseline_equal":exact,"dark":dark,"light":light,
            "stats_before":stats,"stats_after":engine.backend.0.stats()}),
        );
        storage::atomic_json(&dir.join("rounds.json"), &rounds)?;
        if !pass {
            return Err("Recovery did not pass; inspect journal/evidence before continuing".into());
        }
    }
    let median = |key: &str| {
        let mut values: Vec<_> = rounds
            .iter()
            .map(|r| r["light"][key].as_f64().unwrap())
            .collect();
        values.sort_by(f64::total_cmp);
        values[1]
    };
    let result = serde_json::json!({"pass":true,"scope":"One allowlisted external display; no UI, cursor, WMI, or HDR",
        "ack_boundary":"Engine start to all-write callback; excludes actual UI/physical response",
        "light_ack_median_ms":median("presentation_ms"),"light_total_median_ms":median("total_ms"),
        "light_hardware_median_ms":median("elapsed_ms"),"rounds":rounds});
    storage::atomic_json(&dir.join("results.json"), &result)?;
    println!("{result}");
    Ok(())
}
