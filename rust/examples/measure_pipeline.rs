//! One explicitly selected physical screen; compares scan/journal policies on
//! the same Native implementation. Never runs an unrestricted hardware scan.
use sog::{
    config::{Config, CursorMode, DimOrder},
    engine::{
        measurement_config, Backend, Change, ChangeResult, Device, Engine, FileJournal, Inventory,
        Journal, JournalStore, Kind, ScanRequest,
    },
    native::Native,
    storage,
};
use std::{cell::Cell, path::PathBuf, rc::Rc, time::Instant};

struct CountedJournal {
    inner: FileJournal,
    saves: Rc<Cell<usize>>,
}
impl JournalStore for CountedJournal {
    fn load(&self) -> Result<Option<Journal>, String> {
        self.inner.load()
    }
    fn save(&self, journal: &Journal) -> Result<(), String> {
        self.saves.set(self.saves.get() + 1);
        self.inner.save(journal)
    }
    fn clear(&self) -> Result<(), String> {
        self.inner.clear()
    }
}
struct Compared {
    native: Native,
    control: bool,
}
impl Backend for Compared {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.native.scan()
    }
    fn scan_required(&mut self, request: ScanRequest) -> Result<Inventory, String> {
        self.native.scan_required(request)
    }
    fn prepare_restore(&mut self, entries: &[Device], reuse: bool) -> Result<Vec<String>, String> {
        self.native.prepare_restore(entries, reuse && !self.control)
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        self.native.read(id, kind)
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        self.native.write(id, kind, value)
    }
    fn cursor(&mut self, _hidden: bool) -> Result<(), String> {
        Err("This selected-screen measurement must never change the cursor".into())
    }
    fn remote_active(&self) -> bool {
        false
    }
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        self.native.apply(changes, current)
    }
}
fn median(mut numbers: Vec<f64>) -> f64 {
    numbers.sort_by(f64::total_cmp);
    numbers[numbers.len() / 2]
}
fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().ok_or("Absolute evidence directory required")?);
    let id = args
        .next()
        .ok_or("An exact ddc: device ID is required")?
        .to_lowercase();
    if !dir.is_absolute() || !id.starts_with("ddc:") || args.next().is_some() {
        return Err("Pass an absolute directory and exactly one ddc: device ID".into());
    }
    let native = Native::restricted_to(vec![id.clone()])?;
    let saves = Rc::new(Cell::new(0));
    let journal = CountedJournal {
        inner: FileJournal(dir.join("recovery.json")),
        saves: saves.clone(),
    };
    let mut engine = Engine::new(
        Compared {
            native,
            control: false,
        },
        journal,
    )?;
    engine.light(|| true)?;
    if engine.has_pending_recovery() {
        return Err("Prior restoration remains pending; no new measurement".into());
    }
    let request = ScanRequest {
        external: true,
        internal: false,
        hdr: false,
    };
    let before = engine.backend.scan_required(request)?;
    if before.devices.len() != 1
        || before.devices[0].id != id
        || before.devices[0].kind != Kind::External
    {
        return Err("Allowlist must resolve to exactly the requested external display".into());
    }
    storage::atomic_json(&dir.join("before.json"), &before)?;
    let original = Config {
        dim_external: true,
        dim_internal: false,
        hdr_off: false,
        cursor_mode: CursorMode::Keep,
        dim_order: DimOrder::BlackFirst,
        ..Config::default()
    };
    let mut rounds = vec![];
    let mut failure = false;
    for control in [true, false, false, true, true, false] {
        engine.backend.control = control;
        let cfg = if control {
            original.clone()
        } else {
            measurement_config(&original)
        };
        let save_start = saves.get();
        let stats_before =
            serde_json::to_value(engine.backend.native.stats()).map_err(|e| e.to_string())?;
        let start = Instant::now();
        let dark = engine.dark(&cfg, || true);
        // Always restore, including a failed/cancelled dark phase.
        let mut light = engine.light_after_measure(|| true);
        if engine.has_pending_recovery() {
            light = engine.light(|| true);
        }
        let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
        let confirmed = |out: &Result<sog::engine::Outcome, String>| {
            out.as_ref().is_ok_and(|o| {
                o.channels == 1 && o.changes.iter().all(|c| matches!(c.result, Ok(true)))
            })
        };
        let pass = confirmed(&dark) && confirmed(&light) && !engine.has_pending_recovery();
        rounds.push(serde_json::json!({"policy":if control {"full_rescan_control"} else {"optimized"},
            "wall_ms":wall_ms,"journal_saves":saves.get()-save_start,"stats_before":stats_before,
            "stats_after":engine.backend.native.stats(),"dark":dark.as_ref().ok(),"dark_error":dark.as_ref().err(),
            "light":light.as_ref().ok(),"light_error":light.as_ref().err(),"pass":pass}));
        storage::atomic_json(&dir.join("rounds.json"), &rounds)?;
        if !pass {
            failure = true;
            break;
        }
    }
    if engine.has_pending_recovery() {
        return Err(
            "Recovery is retained. Restore this exact allowlisted device before proceeding".into(),
        );
    }
    let after = engine.backend.scan_required(request)?;
    storage::atomic_json(&dir.join("after.json"), &after)?;
    let same = after.devices.len() == 1
        && after.devices[0].id == id
        && after.devices[0].value == before.devices[0].value;
    let values = |policy: &str| {
        rounds
            .iter()
            .filter(|r| r["policy"] == policy)
            .map(|r| r["wall_ms"].as_f64().unwrap())
            .collect::<Vec<_>>()
    };
    let control = values("full_rescan_control");
    let optimized = values("optimized");
    let summary = if !control.is_empty() && !optimized.is_empty() {
        let a = median(control);
        let b = median(optimized);
        serde_json::json!({"control_wall_median_ms":a,"optimized_wall_median_ms":b,"saved_ms":a-b,"saved_percent":(a-b)/a*100.0})
    } else {
        serde_json::Value::Null
    };
    let pass = !failure && same;
    storage::atomic_json(
        &dir.join("results.json"),
        &serde_json::json!({"pass":pass,"device_id":id,
        "scope":"One explicitly allowlisted DDC display only; no WMI/HDR/cursor/overlay",
        "comparison":"Same optimized Native backend, full-rescan/two-journal control versus optimized policy; not the original 0.2 binary",
        "rounds":rounds,"summary":summary,"baselines_equal":same,"pending_recovery":false}),
    )?;
    println!("{}", summary);
    if pass {
        Ok(())
    } else {
        Err("Selected display measurement failed; inspect evidence".into())
    }
}
