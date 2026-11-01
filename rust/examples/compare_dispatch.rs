//! Controlled serial/parallel comparison, with the same Native calls and recovery journal.
use sog::{
    config::{Config, CursorMode},
    engine::{Backend, Change, ChangeResult, Engine, FileJournal, Inventory, Kind},
    native::Native,
    storage,
};
use std::{path::PathBuf, time::Instant};

struct Compared {
    native: Native,
    serial: bool,
}
impl Backend for Compared {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.native.scan()
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        self.native.write(id, kind, value)
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        self.native.read(id, kind)
    }
    fn cursor(&mut self, hidden: bool) -> Result<(), String> {
        self.native.cursor(hidden)
    }
    fn remote_active(&self) -> bool {
        false
    }
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        if !self.serial {
            return self.native.apply(changes, current);
        }
        let started = Instant::now();
        let mut results = Vec::new();
        for change in changes {
            let offset = started.elapsed().as_secs_f64() * 1000.0;
            let mut one = self.native.apply(std::slice::from_ref(change), current);
            for result in &mut one {
                result.started_ms += offset;
            }
            results.extend(one);
        }
        results
    }
}
fn main() -> Result<(), String> {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Pass an absolute evidence directory")?,
    );
    if !out.is_absolute() {
        return Err("Absolute evidence directory required".into());
    }
    let mut engine = Engine::new(
        Compared {
            native: Native::new(false),
            serial: false,
        },
        FileJournal(out.join("recovery.json")),
    )?;
    engine.light(|| true)?;
    if engine.has_pending_recovery() {
        return Err("Prior recovery is still pending".into());
    }
    let before = engine.backend.scan()?;
    storage::atomic_json(&out.join("before.json"), &before)?;
    let cfg = Config {
        cursor_mode: CursorMode::Keep,
        hdr_off: false,
        ..Config::default()
    };
    let mut rounds = Vec::new();
    for serial in [true, false, false, true] {
        engine.backend.serial = serial;
        let dark = engine.dark(&cfg, || true);
        let light = engine.light(|| true);
        if engine.has_pending_recovery() {
            engine.light(|| true)?;
        }
        let dark = dark?;
        let light = light?;
        let pass = !engine.has_pending_recovery()
            && dark.warnings.is_empty()
            && light.warnings.is_empty()
            && dark.channels == 3
            && light.channels == 3;
        println!("{}: dark batch={}ms, light batch={}ms, dark longest lane={:.1}ms, light longest lane={:.1}ms, pass={pass}",
            if serial{"serial"}else{"parallel"},dark.elapsed_ms,light.elapsed_ms,
            dark.changes.iter().map(|c|c.elapsed_ms).fold(0.0,f64::max),
            light.changes.iter().map(|c|c.elapsed_ms).fold(0.0,f64::max));
        rounds.push(serde_json::json!({"serial":serial,"dark":dark,"light":light,"pass":pass}));
    }
    let after = engine.backend.scan()?;
    let equal = before.devices.iter().all(|d| {
        after
            .devices
            .iter()
            .any(|a| a.id == d.id && a.kind == d.kind && a.value == d.value)
    });
    storage::atomic_json(&out.join("after.json"), &after)?;
    let pass = equal && rounds.iter().all(|r| r["pass"] == true);
    storage::atomic_json(
        &out.join("results.json"),
        &serde_json::json!({"real_hardware":true,"pass":pass,"baselines_equal":equal,"rounds":rounds,
        "scope":"Controlled dispatch comparison using the same Native backend; not a successful GUI measurement of the supplied 0.1 EXE"}),
    )?;
    if pass {
        Ok(())
    } else {
        Err("Comparison failed; inspect retained evidence and recovery journal".into())
    }
}
