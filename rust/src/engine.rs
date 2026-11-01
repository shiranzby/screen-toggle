use crate::{
    config::{Config, CursorMode, DimOrder},
    storage,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Hdr,
    Internal,
    External,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub min: u32,
    pub value: u32,
    pub max: u32,
}
#[derive(Default, Debug, Serialize)]
pub struct Inventory {
    pub devices: Vec<Device>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct Change {
    pub device: Device,
    pub value: u32,
}
#[derive(Debug, Serialize)]
pub struct ChangeResult {
    pub id: String,
    pub kind: Kind,
    pub stage: &'static str,
    /// Both timestamps use the start of this apply batch, including scheduling delay.
    pub started_ms: f64,
    pub elapsed_ms: f64,
    pub pre_read_ms: f64,
    pub write_ms: f64,
    /// Cumulative time in confirmation reads, including failed attempts.
    pub verify_ms: f64,
    pub verify_reads: usize,
    pub wait_ms: f64,
    /// true is independently confirmed; false means superseded before confirmation.
    pub result: Result<bool, String>,
}
impl ChangeResult {
    pub(crate) fn completed(
        change: &Change,
        batch_start: Instant,
        started: Instant,
        result: Result<bool, String>,
    ) -> Self {
        Self {
            id: change.device.id.clone(),
            kind: change.device.kind,
            stage: if change.device.kind == Kind::Hdr {
                "hdr"
            } else {
                "brightness"
            },
            started_ms: started.duration_since(batch_start).as_secs_f64() * 1000.0,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            pre_read_ms: 0.0,
            write_ms: 0.0,
            verify_ms: 0.0,
            verify_reads: 0,
            wait_ms: 0.0,
            result,
        }
    }
}
pub trait Backend {
    fn scan(&mut self) -> Result<Inventory, String>;
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String>;
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String>;
    fn cursor(&mut self, hidden: bool) -> Result<(), String>;
    /// The UI adapter applies the cursor and overlay in one UI-thread dispatch.
    /// A recovery journal has already committed before this is called.
    fn present(&mut self, dark: bool, hidden: bool) -> Result<(), String> {
        if hidden {
            self.cursor(dark)?;
        }
        Ok(())
    }
    /// True only for positive evidence of an active incoming remote session.
    fn remote_active(&self) -> bool;
    /// Return only after all changes have stopped using the backend's resources.
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        let batch_start = Instant::now();
        changes
            .iter()
            .map(|change| {
                confirm_change(
                    self,
                    change,
                    current,
                    batch_start,
                    |backend| backend.read(&change.device.id, change.device.kind),
                    |backend, value| backend.write(&change.device.id, change.device.kind, value),
                )
            })
            .collect()
    }
}
pub(crate) fn confirm_change<T: ?Sized>(
    state: &mut T,
    change: &Change,
    current: &(dyn Fn() -> bool + Sync),
    batch_start: Instant,
    mut read: impl FnMut(&mut T) -> Result<u32, String>,
    mut write: impl FnMut(&mut T, u32) -> Result<(), String>,
) -> ChangeResult {
    let started = Instant::now();
    let mut pre_read_ms = 0.0;
    let mut write_ms = 0.0;
    let mut verify_ms = 0.0;
    let mut verify_reads = 0;
    let mut wait_ms = 0.0;
    let result = (|| {
        if !current() {
            return Ok(false);
        }
        let call = Instant::now();
        let actual = read(state);
        pre_read_ms = call.elapsed().as_secs_f64() * 1000.0;
        // The read can block while a newer command changes the generation.
        if !current() {
            return Ok(false);
        }
        if actual.ok() == Some(change.value) {
            return Ok(true);
        }
        let call = Instant::now();
        let written = write(state, change.value);
        write_ms = call.elapsed().as_secs_f64() * 1000.0;
        written?;
        let start = Instant::now();
        loop {
            if !current() {
                return Ok(false);
            }
            let call = Instant::now();
            let actual = read(state);
            verify_ms += call.elapsed().as_secs_f64() * 1000.0;
            verify_reads += 1;
            let actual = actual?;
            if !current() {
                return Ok(false);
            }
            // Zero/floor is a valid baseline too; require the exact original value.
            if actual == change.value {
                return Ok(true);
            }
            if start.elapsed() >= Duration::from_millis(600) {
                return Err(format!("回读超时（目标 {}，实际 {actual}）", change.value));
            }
            let wait = Instant::now();
            std::thread::sleep(Duration::from_millis(20));
            wait_ms += wait.elapsed().as_secs_f64() * 1000.0;
        }
    })()
    .map_err(|error| format!("{}：{error}", change.device.name));
    let mut completed = ChangeResult::completed(change, batch_start, started, result);
    completed.pre_read_ms = pre_read_ms;
    completed.write_ms = write_ms;
    completed.verify_ms = verify_ms;
    completed.verify_reads = verify_reads;
    completed.wait_ms = wait_ms;
    completed
}

/// Apply a value that was already captured in the journal/scan phase.
///
/// Native monitor APIs make a read-before-write round trip.  The journal has
/// already committed the original value and the scan has validated the target
/// range, so a second pre-read only adds latency.  The write is still followed
/// by an exact, cancellable read-back confirmation.
pub(crate) fn confirm_change_after_snapshot<T: ?Sized>(
    state: &mut T,
    change: &Change,
    current: &(dyn Fn() -> bool + Sync),
    batch_start: Instant,
    mut read: impl FnMut(&mut T) -> Result<u32, String>,
    mut write: impl FnMut(&mut T, u32) -> Result<(), String>,
) -> ChangeResult {
    let started = Instant::now();
    let write_start = Instant::now();
    let mut write_ms = 0.0;
    let mut verify_ms = 0.0;
    let mut verify_reads = 0;
    let mut wait_ms = 0.0;
    let result = (|| {
        if !current() {
            return Ok(false);
        }
        let written = write(state, change.value);
        write_ms = write_start.elapsed().as_secs_f64() * 1000.0;
        written?;
        let verify_start = Instant::now();
        loop {
            if !current() {
                return Ok(false);
            }
            let call = Instant::now();
            let actual = read(state);
            verify_ms += call.elapsed().as_secs_f64() * 1000.0;
            verify_reads += 1;
            let actual = actual?;
            if !current() {
                return Ok(false);
            }
            if actual == change.value {
                return Ok(true);
            }
            if verify_start.elapsed() >= Duration::from_millis(600) {
                return Err(format!("回读超时（目标 {}，实际 {actual}）", change.value));
            }
            let wait_start = Instant::now();
            std::thread::sleep(Duration::from_millis(20));
            wait_ms += wait_start.elapsed().as_secs_f64() * 1000.0;
        }
    })();
    let result = result.map_err(|error| format!("{}：{error}", change.device.name));
    let mut completed = ChangeResult::completed(change, batch_start, started, result);
    completed.write_ms = write_ms;
    completed.verify_ms = verify_ms;
    completed.verify_reads = verify_reads;
    completed.wait_ms = wait_ms;
    completed
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub entries: Vec<Device>,
    pub cursor_hidden: bool,
}
pub trait JournalStore {
    fn load(&self) -> Result<Option<Journal>, String>;
    fn save(&self, journal: &Journal) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}
pub struct FileJournal(pub PathBuf);
impl JournalStore for FileJournal {
    fn load(&self) -> Result<Option<Journal>, String> {
        match std::fs::read_to_string(&self.0) {
            Ok(s) => {
                let j: Journal = serde_json::from_str(&s)
                    .map_err(|e| format!("恢复记录损坏，原文件已保留：{e}"))?;
                if j.version != 1
                    || j.entries.iter().any(|d| {
                        d.id.is_empty()
                            || d.min > d.value
                            || d.value > d.max
                            || (d.kind == Kind::Hdr && (d.min != 0 || d.max != 1))
                    })
                {
                    return Err("恢复记录无效，不能猜测屏幕原状态".into());
                }
                Ok(Some(j))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("无法读取恢复记录：{e}")),
        }
    }
    fn save(&self, j: &Journal) -> Result<(), String> {
        storage::atomic_json(&self.0, j)
    }
    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("无法清除恢复记录：{e}")),
        }
    }
}
#[derive(Debug, Default, Serialize)]
pub struct Outcome {
    pub superseded: bool,
    pub warnings: Vec<String>,
    pub channels: usize,
    /// Hardware batch wall time, excluding discovery and journal persistence.
    pub elapsed_ms: u32,
    /// Time until the slowest write returned, before exact read-back confirmation.
    pub write_ms: u32,
    /// Discovery/cursor preparation and journal persistence outside the batches.
    pub prepare_ms: u32,
    pub total_ms: u32,
    pub changes: Vec<ChangeResult>,
}
pub struct Engine<B, J> {
    pub backend: B,
    pub store: J,
    journal: Option<Journal>,
}
impl<B: Backend, J: JournalStore> Engine<B, J> {
    pub fn new(backend: B, store: J) -> Result<Self, String> {
        let journal = store.load()?;
        Ok(Self {
            backend,
            store,
            journal,
        })
    }
    pub fn has_pending_recovery(&self) -> bool {
        self.journal.is_some()
    }
    pub fn dark(
        &mut self,
        cfg: &Config,
        current: impl Fn() -> bool + Sync,
    ) -> Result<Outcome, String> {
        let total = Instant::now();
        cfg.validate()?;
        if self.journal.is_some() {
            let restored = self.light(&current)?;
            if restored.superseded {
                return Ok(restored);
            }
            if self.journal.is_some() {
                return Err(format!(
                    "先恢复上次屏幕状态，再重新熄屏。{}",
                    restored.warnings.join("；")
                ));
            }
        }
        let mut out = Outcome::default();
        let hide = match cfg.cursor_mode {
            CursorMode::Keep => false,
            CursorMode::Hide => true,
            CursorMode::Auto => !self.backend.remote_active(),
        };
        if !current() {
            out.superseded = true;
            out.prepare_ms = milliseconds(total.elapsed());
            out.total_ms = out.prepare_ms;
            return Ok(out);
        }
        let mut journal = Journal {
            version: 1,
            entries: Vec::new(),
            cursor_hidden: hide,
        };
        if cfg.dim_order != DimOrder::DimFirst {
            // Preserve the original tool's immediate visual response. Commit
            // the cursor recovery first; monitor discovery is still read-only.
            self.store.save(&journal)?;
            self.journal = Some(journal.clone());
        }
        if !current() {
            out.superseded = true;
            out.prepare_ms = milliseconds(total.elapsed());
            out.total_ms = out.prepare_ms;
            return Ok(out);
        }
        if cfg.dim_order != DimOrder::DimFirst {
            self.backend.present(true, hide)?;
        }
        if !current() {
            out.superseded = true;
            out.prepare_ms = milliseconds(total.elapsed());
            out.total_ms = out.prepare_ms;
            return Ok(out);
        }
        let inventory = self.backend.scan()?;
        out.warnings = inventory.warnings;
        journal.entries = inventory
            .devices
            .into_iter()
            .filter(|d| match d.kind {
                Kind::External => cfg.dim_external,
                Kind::Internal => cfg.dim_internal,
                Kind::Hdr => cfg.hdr_off && d.value != 0,
            })
            .collect();
        if journal
            .entries
            .iter()
            .any(|d| d.min > d.value || d.value > d.max)
        {
            return Err("亮度回读超出设备范围，已中止调光".into());
        }
        if !current() {
            out.superseded = true;
            out.prepare_ms = milliseconds(total.elapsed());
            out.total_ms = out.prepare_ms;
            return Ok(out);
        }
        // Actual hardware baselines must be durable before the first write.
        // If this second commit fails, only the early cursor record remains.
        self.store.save(&journal)?;
        self.journal = Some(journal.clone());
        out.prepare_ms = milliseconds(total.elapsed());
        let start = Instant::now();
        self.apply_groups(&journal.entries, false, &current, &mut out);
        out.elapsed_ms = milliseconds(start.elapsed());
        if cfg.dim_order == DimOrder::DimFirst && current() {
            self.backend.present(true, hide)?;
        }
        out.superseded |= !current();
        if out.channels == 0 && (cfg.dim_internal || cfg.dim_external) {
            out.warnings
                .push("未完成硬件调光，当前仅使用黑色覆盖窗".into());
        }
        out.total_ms = milliseconds(total.elapsed());
        Ok(out)
    }
    pub fn light(&mut self, current: impl Fn() -> bool + Sync) -> Result<Outcome, String> {
        let total = Instant::now();
        let Some(journal) = self.journal.clone() else {
            return Ok(Outcome::default());
        };
        let mut out = Outcome::default();
        let mut remaining = journal.clone();
        // Reacquire by identity, never by monitor order or by a saved HANDLE.
        if !journal.entries.is_empty() {
            let inventory = self.backend.scan()?;
            out.warnings.extend(inventory.warnings);
        }
        out.prepare_ms = milliseconds(total.elapsed());
        let start = Instant::now();
        self.apply_groups(&journal.entries, true, &current, &mut out);
        out.elapsed_ms = milliseconds(start.elapsed());
        // Keep the cursor hidden through the whole restore (including any UI
        // reveal delay). The UI acknowledges both changes together.
        let mut presentation_confirmed = false;
        if current() {
            let presentation = Instant::now();
            match self.backend.present(false, journal.cursor_hidden) {
                Ok(()) => {
                    remaining.cursor_hidden = false;
                    presentation_confirmed = true;
                }
                Err(e) => out.warnings.push(e),
            }
            out.prepare_ms = out
                .prepare_ms
                .saturating_add(milliseconds(presentation.elapsed()));
        }
        out.superseded |= !current();
        for result in &out.changes {
            if matches!(result.result, Ok(true)) {
                remaining
                    .entries
                    .retain(|d| d.id != result.id || d.kind != result.kind);
            }
        }
        let persist = Instant::now();
        if remaining.entries.is_empty() && !remaining.cursor_hidden && presentation_confirmed {
            self.store.clear()?;
            self.journal = None;
        } else {
            self.store.save(&remaining)?;
            self.journal = Some(remaining);
        }
        out.prepare_ms = out
            .prepare_ms
            .saturating_add(milliseconds(persist.elapsed()));
        out.total_ms = milliseconds(total.elapsed());
        Ok(out)
    }
    fn apply_groups(
        &mut self,
        devices: &[Device],
        restoring: bool,
        current: &(dyn Fn() -> bool + Sync),
        out: &mut Outcome,
    ) {
        let (hdr, brightness): (Vec<_>, Vec<_>) = devices
            .iter()
            .map(|device| Change {
                device: device.clone(),
                value: if restoring { device.value } else { device.min },
            })
            .partition(|change| change.device.kind == Kind::Hdr);
        // HDR may change a display's topology. Finish its entire batch before
        // dispatching brightness, and never overlap separate Engine operations.
        for changes in [hdr, brightness] {
            if !current() {
                out.superseded = true;
                break;
            }
            if changes.is_empty() {
                continue;
            }
            let group_started = out.elapsed_ms;
            let batch = Instant::now();
            for result in self.backend.apply(&changes, current) {
                out.write_ms = out.write_ms.max(
                    (f64::from(group_started) + result.started_ms + result.write_ms)
                        .ceil()
                        .min(u32::MAX as f64) as u32,
                );
                match &result.result {
                    Ok(true) => out.channels += 1,
                    Ok(false) => out.superseded = true,
                    Err(error) => out.warnings.push(error.clone()),
                }
                out.changes.push(result);
            }
            out.elapsed_ms = out.elapsed_ms.saturating_add(milliseconds(batch.elapsed()));
        }
        out.superseded |= !current();
    }
}
fn milliseconds(duration: Duration) -> u32 {
    duration.as_millis().min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        rc::Rc,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    #[derive(Default)]
    struct MemoryStore {
        saved: RefCell<Option<Journal>>,
        fail: Cell<bool>,
        saves: Cell<usize>,
        fail_on_save: Cell<Option<usize>>,
    }
    impl JournalStore for Rc<MemoryStore> {
        fn load(&self) -> Result<Option<Journal>, String> {
            Ok(self.saved.borrow().clone())
        }
        fn save(&self, j: &Journal) -> Result<(), String> {
            self.saves.set(self.saves.get() + 1);
            if self.fail.get() || self.fail_on_save.get() == Some(self.saves.get()) {
                return Err("disk full".into());
            }
            *self.saved.borrow_mut() = Some(j.clone());
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            if self.fail.get() {
                return Err("disk full".into());
            }
            *self.saved.borrow_mut() = None;
            Ok(())
        }
    }
    struct Fake {
        values: BTreeMap<String, u32>,
        writes: Vec<(String, u32)>,
        fail_id: Option<String>,
        cursor: bool,
        cursor_at_write: Vec<bool>,
        remote: bool,
        cancel: Option<Arc<AtomicBool>>,
        cancel_on_read: Option<Arc<AtomicBool>>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                values: BTreeMap::from([
                    ("same-model/serial-a".into(), 0),
                    ("same-model/serial-b".into(), 76),
                    ("hdr-a".into(), 1),
                ]),
                writes: vec![],
                fail_id: None,
                cursor: false,
                cursor_at_write: vec![],
                remote: false,
                cancel: None,
                cancel_on_read: None,
            }
        }
    }
    impl Backend for Fake {
        fn scan(&mut self) -> Result<Inventory, String> {
            Ok(Inventory {
                devices: self
                    .values
                    .iter()
                    .rev()
                    .map(|(id, &value)| Device {
                        id: id.clone(),
                        name: id.clone(),
                        kind: if id == "hdr-a" {
                            Kind::Hdr
                        } else {
                            Kind::External
                        },
                        min: 0,
                        value,
                        max: if id == "hdr-a" { 1 } else { 100 },
                    })
                    .collect(),
                warnings: vec![],
            })
        }
        fn write(&mut self, id: &str, _kind: Kind, value: u32) -> Result<(), String> {
            if self.fail_id.as_deref() == Some(id) {
                return Err("disconnected".into());
            }
            let actual = self.values.get_mut(id).ok_or("missing identity")?;
            *actual = value;
            self.cursor_at_write.push(self.cursor);
            self.writes.push((id.into(), value));
            if let Some(c) = &self.cancel {
                c.store(false, Ordering::Release);
            }
            Ok(())
        }
        fn read(&mut self, id: &str, _kind: Kind) -> Result<u32, String> {
            if let Some(current) = &self.cancel_on_read {
                current.store(false, Ordering::Release);
            }
            self.values
                .get(id)
                .copied()
                .ok_or("missing identity".into())
        }
        fn cursor(&mut self, hidden: bool) -> Result<(), String> {
            self.cursor = hidden;
            Ok(())
        }
        fn remote_active(&self) -> bool {
            self.remote
        }
    }
    #[test]
    fn baseline_zero_and_duplicate_names_restore_by_identity() {
        let store = Rc::new(MemoryStore::default());
        let mut engine = Engine::new(Fake::default(), store.clone()).unwrap();
        let cfg = Config {
            hdr_off: true,
            cursor_mode: CursorMode::Hide,
            ..Config::default()
        };
        engine.dark(&cfg, || true).unwrap();
        assert_eq!(engine.backend.values["same-model/serial-b"], 0);
        // Simulate a restart: handles/enumeration order and live preferences are irrelevant.
        let mut restarted = Engine::new(engine.backend, store).unwrap();
        restarted.light(|| true).unwrap();
        assert_eq!(restarted.backend.values["same-model/serial-a"], 0);
        assert_eq!(restarted.backend.values["same-model/serial-b"], 76);
        assert_eq!(restarted.backend.values["hdr-a"], 1);
        assert!(!restarted.backend.cursor);
        assert!(!restarted.has_pending_recovery());
    }
    #[test]
    fn failed_journal_does_not_touch_hardware_or_cursor() {
        let store = Rc::new(MemoryStore::default());
        store.fail.set(true);
        let mut e = Engine::new(Fake::default(), store).unwrap();
        assert!(e.dark(&Config::default(), || true).is_err());
        assert!(e.backend.writes.is_empty());
        assert!(!e.backend.cursor);
    }
    #[test]
    fn partial_failure_retains_original_until_retry() {
        let store = Rc::new(MemoryStore::default());
        let mut e = Engine::new(Fake::default(), store.clone()).unwrap();
        e.dark(&Config::default(), || true).unwrap();
        e.backend.fail_id = Some("same-model/serial-b".into());
        assert!(!e.light(|| true).unwrap().warnings.is_empty());
        assert!(e.has_pending_recovery());
        assert_eq!(store.saved.borrow().as_ref().unwrap().entries[0].value, 76);
        e.backend.fail_id = None;
        e.light(|| true).unwrap();
        assert_eq!(e.backend.values["same-model/serial-b"], 76);
        assert!(store.saved.borrow().is_none());
    }
    #[test]
    fn new_intent_interrupts_between_writes_and_final_light_wins() {
        let current = Arc::new(AtomicBool::new(true));
        let fake = Fake {
            cancel: Some(current.clone()),
            ..Fake::default()
        };
        let mut e = Engine::new(fake, Rc::new(MemoryStore::default())).unwrap();
        assert!(
            e.dark(&Config::default(), || current.load(Ordering::Acquire))
                .unwrap()
                .superseded
        );
        assert_eq!(e.backend.writes.len(), 1);
        e.backend.cancel = None;
        e.light(|| true).unwrap();
        assert_eq!(e.backend.values["same-model/serial-b"], 76);
        assert!(!e.has_pending_recovery());
    }
    #[test]
    fn disabled_preferences_do_not_cancel_needed_restoration() {
        let mut e = Engine::new(Fake::default(), Rc::new(MemoryStore::default())).unwrap();
        e.dark(
            &Config {
                cursor_mode: CursorMode::Hide,
                ..Config::default()
            },
            || true,
        )
        .unwrap();
        // The restore API intentionally takes no Config, so changing toggles can't strand a screen.
        e.light(|| true).unwrap();
        assert_eq!(e.backend.values["same-model/serial-b"], 76);
        assert!(!e.backend.cursor);
    }
    #[test]
    fn remote_auto_keeps_cursor() {
        let mut e = Engine::new(
            Fake {
                remote: true,
                ..Fake::default()
            },
            Rc::new(MemoryStore::default()),
        )
        .unwrap();
        e.dark(&Config::default(), || true).unwrap();
        assert!(!e.backend.cursor);
    }
    #[test]
    fn cursor_follows_presentation_order_and_stays_hidden_until_restored() {
        for order in [DimOrder::BlackFirst, DimOrder::Fade, DimOrder::DimFirst] {
            let mut engine = Engine::new(Fake::default(), Rc::new(MemoryStore::default())).unwrap();
            engine
                .dark(
                    &Config {
                        dim_order: order,
                        cursor_mode: CursorMode::Hide,
                        ..Config::default()
                    },
                    || true,
                )
                .unwrap();
            assert!(!engine.backend.cursor_at_write.is_empty());
            assert!(engine
                .backend
                .cursor_at_write
                .iter()
                .all(|&hidden| hidden == (order != DimOrder::DimFirst)));
            assert!(engine.backend.cursor);
            engine.backend.cursor_at_write.clear();
            engine.light(|| true).unwrap();
            assert!(engine.backend.cursor_at_write.iter().all(|&hidden| hidden));
            assert!(!engine.backend.cursor);
            assert!(!engine.has_pending_recovery());
        }
    }
    #[test]
    fn cursor_modes_follow_actual_session_and_restore_pointer() {
        for remote in [false, true] {
            for (mode, hidden) in [
                (CursorMode::Auto, !remote),
                (CursorMode::Hide, true),
                (CursorMode::Keep, false),
            ] {
                let mut engine = Engine::new(
                    Fake {
                        remote,
                        ..Fake::default()
                    },
                    Rc::new(MemoryStore::default()),
                )
                .unwrap();
                engine
                    .dark(
                        &Config {
                            cursor_mode: mode,
                            ..Config::default()
                        },
                        || true,
                    )
                    .unwrap();
                assert_eq!(engine.backend.cursor, hidden);
                engine.light(|| true).unwrap();
                assert!(!engine.backend.cursor);
                assert!(!engine.has_pending_recovery());
            }
        }
    }
    #[test]
    fn snapshot_write_has_no_preread_and_does_not_skip_confirmation() {
        let change = Change {
            device: Device {
                id: "test".into(),
                name: "test".into(),
                kind: Kind::External,
                min: 0,
                value: 75,
                max: 100,
            },
            value: 0,
        };
        let mut calls = Vec::new();
        let result = confirm_change_after_snapshot(
            &mut calls,
            &change,
            &|| true,
            Instant::now(),
            |calls| {
                calls.push("read");
                Ok(0)
            },
            |calls, value| {
                assert_eq!(value, 0);
                calls.push("write");
                Ok(())
            },
        );
        assert_eq!(calls, ["write", "read"]);
        assert_eq!(result.pre_read_ms, 0.0);
        assert_eq!(result.verify_reads, 1);
        assert!(matches!(result.result, Ok(true)));
        let result = confirm_change_after_snapshot(
            &mut (),
            &change,
            &|| true,
            Instant::now(),
            |_| Err("device disappeared".to_string()),
            |_, _| Ok(()),
        );
        assert!(result.result.is_err());
        assert_eq!(result.verify_reads, 1);
    }
    #[test]
    fn generation_changed_during_preread_does_not_write() {
        let current = Arc::new(AtomicBool::new(true));
        let fake = Fake {
            cancel_on_read: Some(current.clone()),
            ..Fake::default()
        };
        let store = Rc::new(MemoryStore::default());
        let mut engine = Engine::new(fake, store.clone()).unwrap();
        let out = engine
            .dark(&Config::default(), || current.load(Ordering::Acquire))
            .unwrap();
        assert!(out.superseded);
        assert!(out
            .changes
            .iter()
            .all(|change| change.write_ms == 0.0 && change.verify_reads == 0));
        assert!(engine.backend.writes.is_empty());
        assert!(store.saved.borrow().is_some());
        engine.backend.cancel_on_read = None;
        engine.light(|| true).unwrap();
        assert!(store.saved.borrow().is_none());
    }
    #[test]
    fn restore_keeps_only_unconfirmed_device_after_partial_batch_failure() {
        let store = Rc::new(MemoryStore::default());
        let mut fake = Fake::default();
        fake.values.insert("same-model/serial-a".into(), 25);
        let mut engine = Engine::new(fake, store.clone()).unwrap();
        engine.dark(&Config::default(), || true).unwrap();
        engine.backend.fail_id = Some("same-model/serial-b".into());
        let out = engine.light(|| true).unwrap();
        assert_eq!(engine.backend.values["same-model/serial-a"], 25);
        assert_eq!(engine.backend.values["same-model/serial-b"], 0);
        assert_eq!(out.changes.len(), 2);
        let remaining = store.saved.borrow();
        let entries = &remaining.as_ref().unwrap().entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "same-model/serial-b");
        assert_eq!(entries[0].value, 76);
    }
    struct BatchAudit {
        inner: Fake,
        store: Rc<MemoryStore>,
        batches: Vec<Vec<Kind>>,
        events: Vec<&'static str>,
        cancel_after_apply: Option<Arc<AtomicBool>>,
    }
    impl Backend for BatchAudit {
        fn scan(&mut self) -> Result<Inventory, String> {
            self.events.push("scan");
            self.inner.scan()
        }
        fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
            self.inner.write(id, kind, value)
        }
        fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
            self.inner.read(id, kind)
        }
        fn cursor(&mut self, hidden: bool) -> Result<(), String> {
            self.inner.cursor(hidden)
        }
        fn present(&mut self, dark: bool, hidden: bool) -> Result<(), String> {
            assert!(
                self.store.saved.borrow().is_some(),
                "presentation needs durable recovery"
            );
            self.events.push(if dark {
                "present-dark"
            } else {
                "present-light"
            });
            self.inner.present(dark, hidden)
        }
        fn remote_active(&self) -> bool {
            self.inner.remote_active()
        }
        fn apply(
            &mut self,
            changes: &[Change],
            current: &(dyn Fn() -> bool + Sync),
        ) -> Vec<ChangeResult> {
            let journal = self.store.saved.borrow();
            assert!(journal.is_some());
            assert!(changes.iter().all(|c| journal
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .any(|d| d.id == c.device.id && d.kind == c.device.kind)));
            assert!(
                changes.iter().all(|change| change.device.kind == Kind::Hdr)
                    || changes.iter().all(|change| change.device.kind != Kind::Hdr)
            );
            self.batches
                .push(changes.iter().map(|change| change.device.kind).collect());
            self.events.push("apply");
            let result = self.inner.apply(changes, current);
            if let Some(current) = &self.cancel_after_apply {
                current.store(false, Ordering::Release);
            }
            result
        }
    }
    #[test]
    fn journal_precedes_batches_and_hdr_finishes_before_brightness() {
        let store = Rc::new(MemoryStore::default());
        let backend = BatchAudit {
            inner: Fake::default(),
            store: store.clone(),
            batches: vec![],
            events: vec![],
            cancel_after_apply: None,
        };
        let mut engine = Engine::new(backend, store).unwrap();
        let cfg = Config {
            hdr_off: true,
            ..Config::default()
        };
        let dark = engine.dark(&cfg, || true).unwrap();
        let light = engine.light(|| true).unwrap();
        assert_eq!(
            engine.backend.batches,
            vec![
                vec![Kind::Hdr],
                vec![Kind::External, Kind::External],
                vec![Kind::Hdr],
                vec![Kind::External, Kind::External]
            ]
        );
        assert_eq!(dark.changes.len(), 3);
        assert_eq!(light.changes.len(), 3);
        assert!(dark.total_ms >= dark.elapsed_ms);
        assert!(light.total_ms >= light.elapsed_ms);
        assert!(serde_json::to_string(&dark).unwrap().contains("started_ms"));
    }
    #[test]
    fn black_first_presentation_precedes_discovery_with_recovery_already_saved() {
        for order in [DimOrder::BlackFirst, DimOrder::Fade, DimOrder::DimFirst] {
            let store = Rc::new(MemoryStore::default());
            let backend = BatchAudit {
                inner: Fake::default(),
                store: store.clone(),
                batches: vec![],
                events: vec![],
                cancel_after_apply: None,
            };
            let mut engine = Engine::new(backend, store).unwrap();
            engine
                .dark(
                    &Config {
                        dim_order: order,
                        cursor_mode: CursorMode::Hide,
                        ..Config::default()
                    },
                    || true,
                )
                .unwrap();
            assert_eq!(
                engine.backend.events,
                if order == DimOrder::DimFirst {
                    vec!["scan", "apply", "present-dark"]
                } else {
                    vec!["present-dark", "scan", "apply"]
                }
            );
            engine.light(|| true).unwrap();
            assert!(!engine.has_pending_recovery());
        }
    }
    #[test]
    fn failed_hardware_baseline_commit_retains_only_early_cursor_recovery() {
        let store = Rc::new(MemoryStore::default());
        store.fail_on_save.set(Some(2));
        let mut engine = Engine::new(Fake::default(), store.clone()).unwrap();
        assert!(engine
            .dark(
                &Config {
                    cursor_mode: CursorMode::Hide,
                    ..Config::default()
                },
                || true
            )
            .is_err());
        assert!(engine.backend.cursor);
        assert!(engine.backend.writes.is_empty());
        assert!(store.saved.borrow().as_ref().unwrap().entries.is_empty());
        engine.light(|| true).unwrap();
        assert!(!engine.backend.cursor);
        assert!(!engine.has_pending_recovery());
    }
    #[test]
    fn keep_cursor_cancelled_after_last_confirmation_keeps_pending_presentation() {
        let store = Rc::new(MemoryStore::default());
        let backend = BatchAudit {
            inner: Fake::default(),
            store: store.clone(),
            batches: vec![],
            events: vec![],
            cancel_after_apply: None,
        };
        let mut engine = Engine::new(backend, store.clone()).unwrap();
        engine
            .dark(
                &Config {
                    cursor_mode: CursorMode::Keep,
                    ..Config::default()
                },
                || true,
            )
            .unwrap();
        let current = Arc::new(AtomicBool::new(true));
        engine.backend.cancel_after_apply = Some(current.clone());
        let result = engine.light(|| current.load(Ordering::Acquire)).unwrap();
        assert!(result.superseded);
        assert!(store.saved.borrow().as_ref().unwrap().entries.is_empty());
        assert!(
            engine.has_pending_recovery(),
            "the overlay still needs a current UI acknowledgement"
        );
        assert!(!engine.backend.events.contains(&"present-light"));
        engine.backend.cancel_after_apply = None;
        engine.light(|| true).unwrap();
        assert!(engine.backend.events.contains(&"present-light"));
        assert!(!engine.has_pending_recovery());
    }
    #[test]
    fn call_timings_cover_preread_write_all_verification_reads_and_wait() {
        let change = Change {
            device: Fake::default().scan().unwrap().devices.remove(0),
            value: 0,
        };
        let mut reads = 0;
        let result = confirm_change(
            &mut reads,
            &change,
            &|| true,
            Instant::now(),
            |reads| {
                std::thread::sleep(Duration::from_millis(2));
                *reads += 1;
                Ok(if *reads < 3 { 75 } else { 0 })
            },
            |_, _| {
                std::thread::sleep(Duration::from_millis(2));
                Ok(())
            },
        );
        assert!(matches!(result.result, Ok(true)));
        assert_eq!(reads, 3);
        assert_eq!(result.verify_reads, 2);
        assert!(result.pre_read_ms >= 1.0);
        assert!(result.write_ms >= 1.0);
        assert!(result.verify_ms >= 2.0);
        assert!(result.wait_ms >= 15.0);
        assert!(
            result.elapsed_ms
                >= result.pre_read_ms + result.write_ms + result.verify_ms + result.wait_ms
        );
    }
    #[test]
    fn failed_write_still_records_its_time_without_inventing_verification() {
        let change = Change {
            device: Fake::default().scan().unwrap().devices.remove(0),
            value: 0,
        };
        let result = confirm_change(
            &mut (),
            &change,
            &|| true,
            Instant::now(),
            |_| Ok(75),
            |_, _| {
                std::thread::sleep(Duration::from_millis(2));
                Err("disconnected".into())
            },
        );
        assert!(result.result.unwrap_err().contains("disconnected"));
        assert!(result.write_ms >= 1.0);
        assert_eq!(result.verify_reads, 0);
        assert_eq!(result.verify_ms, 0.0);
        assert_eq!(result.wait_ms, 0.0);
    }
}
