use crate::{
    config::{display_is_selected, Config, CursorMode, DimOrder},
    storage,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanRequest {
    pub external: bool,
    pub internal: bool,
    pub hdr: bool,
}
impl ScanRequest {
    pub const fn all() -> Self {
        Self {
            external: true,
            internal: true,
            hdr: true,
        }
    }
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            external: cfg.dim_external,
            internal: cfg.dim_internal,
            hdr: cfg.hdr_off,
        }
    }
    pub fn from_devices(devices: &[Device]) -> Self {
        Self {
            external: devices.iter().any(|d| d.kind == Kind::External),
            internal: devices.iter().any(|d| d.kind == Kind::Internal),
            hdr: devices.iter().any(|d| d.kind == Kind::Hdr),
        }
    }
    pub fn includes(self, kind: Kind) -> bool {
        match kind {
            Kind::External => self.external,
            Kind::Internal => self.internal,
            Kind::Hdr => self.hdr,
        }
    }
}
/// Measurement has no presentation side effects. Avoid the early UI-only
/// journal while retaining the full baseline commit before hardware writes.
pub fn measurement_config(cfg: &Config) -> Config {
    let mut measurement = cfg.clone();
    measurement.cursor_mode = CursorMode::Keep;
    measurement.dim_order = DimOrder::DimFirst;
    measurement
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
    /// One on the normal path; a recovery fault permits one fresh-binding retry.
    pub attempts: usize,
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
            attempts: 1,
            result,
        }
    }
}
pub struct PresentationAck {
    pub result: Result<(), String>,
    pub at: Instant,
}
pub struct RestoreBatch {
    pub changes: Vec<ChangeResult>,
    pub presentation: Option<PresentationAck>,
}
/// Every requested change counts, including unavailable or rejected devices.
/// Notification is only for successful writes, never a readiness assertion.
pub(crate) struct WriteAcknowledgements<'a> {
    remaining: AtomicUsize,
    notified: AtomicBool,
    current: &'a (dyn Fn() -> bool + Sync),
    all_written: &'a (dyn Fn() + Sync),
}
impl<'a> WriteAcknowledgements<'a> {
    pub(crate) fn new(
        count: usize,
        current: &'a (dyn Fn() -> bool + Sync),
        all_written: &'a (dyn Fn() + Sync),
    ) -> Self {
        Self {
            remaining: AtomicUsize::new(count),
            notified: AtomicBool::new(false),
            current,
            all_written,
        }
    }
    pub(crate) fn success(&self) {
        if !(self.current)() {
            return;
        }
        loop {
            let remaining = self.remaining.load(Ordering::Acquire);
            if remaining == 0 {
                return;
            }
            if self
                .remaining
                .compare_exchange(
                    remaining,
                    remaining - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                if remaining == 1 && (self.current)() && !self.notified.swap(true, Ordering::AcqRel)
                {
                    (self.all_written)();
                }
                return;
            }
        }
    }
}
pub trait Backend {
    fn scan(&mut self) -> Result<Inventory, String>;
    fn scan_required(&mut self, request: ScanRequest) -> Result<Inventory, String> {
        let mut inventory = self.scan()?;
        inventory.devices.retain(|d| request.includes(d.kind));
        Ok(inventory)
    }
    /// Generic/simulated backends filter their inventory. Native overrides this
    /// before device access; a post-scan filter would not isolate real hardware.
    fn scan_selected(
        &mut self,
        request: ScanRequest,
        selected: Option<&[String]>,
    ) -> Result<Inventory, String> {
        let mut inventory = self.scan_required(request)?;
        inventory
            .devices
            .retain(|device| display_is_selected(&device.id, selected));
        Ok(inventory)
    }
    /// Reuse is permitted only for the immediate restore in one measurement.
    /// Default backends keep the full refresh; Native validates topology first.
    fn prepare_restore(
        &mut self,
        entries: &[Device],
        _allow_reuse: bool,
    ) -> Result<Vec<String>, String> {
        self.scan_required(ScanRequest::from_devices(entries))
            .map(|i| i.warnings)
    }
    /// HDR can invalidate display resources. Rebind brightness by the journal's
    /// identities without replacing the saved values with current dim values.
    fn rebind_brightness(&mut self, entries: &[Device]) -> Result<Vec<String>, String> {
        self.prepare_restore(entries, false)
    }
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
    fn apply_internal(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        _all_written: Option<&(dyn Fn() + Sync)>,
    ) -> Vec<ChangeResult> {
        self.apply(changes, current)
    }
    /// Native notifies after all writes, while confirmation reads continue.
    /// Generic backends safely fall back to their complete apply contract.
    fn apply_with_write_ack(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        all_written: &(dyn Fn() + Sync),
    ) -> Vec<ChangeResult> {
        let results = self.apply(changes, current);
        if !changes.is_empty()
            && results.len() == changes.len()
            && results.iter().all(|r| matches!(r.result, Ok(true)))
            && current()
        {
            all_written();
        }
        results
    }
    fn apply_restore(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        _hidden: bool,
    ) -> RestoreBatch {
        RestoreBatch {
            changes: self.apply(changes, current),
            presentation: None,
        }
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
    read: impl FnMut(&mut T) -> Result<u32, String>,
    write: impl FnMut(&mut T, u32) -> Result<(), String>,
) -> ChangeResult {
    confirm_change_after_snapshot_with_ack(state, change, current, batch_start, read, write, &|| {})
}
pub(crate) fn confirm_change_after_snapshot_with_ack<T: ?Sized>(
    state: &mut T,
    change: &Change,
    current: &(dyn Fn() -> bool + Sync),
    batch_start: Instant,
    mut read: impl FnMut(&mut T) -> Result<u32, String>,
    mut write: impl FnMut(&mut T, u32) -> Result<(), String>,
    written: &(dyn Fn() + Sync),
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
        let write_result = write(state, change.value);
        write_ms = write_start.elapsed().as_secs_f64() * 1000.0;
        write_result?;
        if !current() {
            return Ok(false);
        }
        // Posting presentation must not wait for UI or defer this lane's read.
        written();
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
impl Journal {
    fn validate(&self) -> Result<(), String> {
        let mut identities = BTreeSet::new();
        if self.version != 1
            || self.entries.iter().any(|d| {
                d.id.is_empty()
                    || !identities.insert((d.kind, d.id.to_ascii_lowercase()))
                    || d.min > d.value
                    || d.value > d.max
                    || (d.kind == Kind::Hdr && (d.min != 0 || d.max != 1))
                    || (d.kind == Kind::Internal && (d.min != 0 || d.max != 100))
            })
        {
            return Err("恢复记录的设备身份或亮度范围无效，原始记录已保留".into());
        }
        Ok(())
    }
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
                j.validate()?;
                Ok(Some(j))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("无法读取恢复记录：{e}")),
        }
    }
    fn save(&self, j: &Journal) -> Result<(), String> {
        j.validate()?;
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
    /// Engine start to acknowledged UI transition; not physical panel response.
    pub presentation_ms: Option<f64>,
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
            out.presentation_ms = Some(total.elapsed().as_secs_f64() * 1000.0);
        }
        if !current() {
            out.superseded = true;
            out.prepare_ms = milliseconds(total.elapsed());
            out.total_ms = out.prepare_ms;
            return Ok(out);
        }
        let inventory = self.backend.scan_selected(
            ScanRequest::from_config(cfg),
            cfg.selected_displays.as_deref(),
        )?;
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
        journal.validate()?;
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
        self.apply_groups(&journal.entries, false, &current, &mut out, None);
        if cfg.dim_order == DimOrder::DimFirst && current() {
            self.backend.present(true, hide)?;
            out.presentation_ms = Some(total.elapsed().as_secs_f64() * 1000.0);
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
        let first = self.restore(&current, false)?;
        if !current()
            || !self.has_pending_recovery()
            || !first.changes.iter().any(|r| r.result.is_err())
        {
            return Ok(first);
        }
        // Healthy restores take one pass. Only failed devices/dependencies
        // remain in the journal and get one fresh binding before a retry.
        // The successful-write presentation of the first pass is never delayed.
        match self.restore(&current, false) {
            Ok(retry) => Ok(merge_restore_attempts(first, retry)),
            Err(error) => {
                let mut first = first;
                first.warnings.push(format!("重新绑定恢复未完成：{error}"));
                Ok(first)
            }
        }
    }
    /// The caller has just completed this measurement's dark phase, including
    /// joining every hardware lane; cancellation still requires restoration.
    pub fn light_after_measure(
        &mut self,
        current: impl Fn() -> bool + Sync,
    ) -> Result<Outcome, String> {
        self.restore(current, true)
    }
    fn restore(
        &mut self,
        current: impl Fn() -> bool + Sync,
        allow_reuse: bool,
    ) -> Result<Outcome, String> {
        let total = Instant::now();
        let Some(journal) = self.journal.clone() else {
            return Ok(Outcome::default());
        };
        let mut out = Outcome::default();
        let mut remaining = journal.clone();
        // Reacquire by identity, never by monitor order or by a saved HANDLE.
        let hdr_entries: Vec<_> = journal
            .entries
            .iter()
            .filter(|d| d.kind == Kind::Hdr)
            .cloned()
            .collect();
        let prepare = if hdr_entries.is_empty() {
            &journal.entries
        } else {
            &hdr_entries
        };
        if !prepare.is_empty() {
            out.warnings
                .extend(self.backend.prepare_restore(prepare, allow_reuse)?);
        }
        out.prepare_ms = milliseconds(total.elapsed());
        let early = self.apply_groups(
            &journal.entries,
            true,
            &current,
            &mut out,
            if allow_reuse {
                None
            } else {
                Some(journal.cursor_hidden)
            },
        );
        let mut presentation_confirmed = false;
        if let Some(ack) = early {
            match ack.result {
                Ok(()) => {
                    remaining.cursor_hidden = false;
                    presentation_confirmed = true;
                    out.presentation_ms = Some(ack.at.duration_since(total).as_secs_f64() * 1000.0);
                }
                Err(e) => out.warnings.push(e),
            }
        } else if current() {
            // Delay mode and failed-write batches retain the complete-confirm
            // fallback. Both visible transitions still share one UI dispatch.
            let presentation = Instant::now();
            match self.backend.present(false, journal.cursor_hidden) {
                Ok(()) => {
                    remaining.cursor_hidden = false;
                    presentation_confirmed = true;
                    out.presentation_ms = Some(total.elapsed().as_secs_f64() * 1000.0);
                }
                Err(e) => out.warnings.push(e),
            }
            out.prepare_ms = out
                .prepare_ms
                .saturating_add(milliseconds(presentation.elapsed()));
        }
        out.superseded |= !current();
        // A later HDR retry can reset brightness that was already confirmed.
        // Keep those original values until every HDR dependency is restored.
        let hdr_pending = journal.entries.iter().any(|device| {
            device.kind == Kind::Hdr
                && !out.changes.iter().any(|result| {
                    result.id == device.id
                        && result.kind == Kind::Hdr
                        && matches!(result.result, Ok(true))
                })
        });
        for result in &out.changes {
            if matches!(result.result, Ok(true)) && !(hdr_pending && result.kind != Kind::Hdr) {
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
        reveal: Option<bool>,
    ) -> Option<PresentationAck> {
        let (hdr, brightness): (Vec<_>, Vec<_>) = devices
            .iter()
            .map(|device| Change {
                device: device.clone(),
                value: if restoring { device.value } else { device.min },
            })
            .partition(|change| change.device.kind == Kind::Hdr);
        // HDR may change a display's topology. Finish its entire batch before
        // dispatching brightness, and never overlap separate Engine operations.
        let mut presentation = None;
        let mut hdr_confirmed = true;
        let hdr_requested = !hdr.is_empty();
        for (stage, changes) in [hdr, brightness].into_iter().enumerate() {
            if !current() {
                out.superseded = true;
                break;
            }
            if changes.is_empty() {
                continue;
            }
            if stage == 1 && hdr_requested {
                let entries: Vec<_> = changes.iter().map(|c| c.device.clone()).collect();
                let started = Instant::now();
                let rebound = self.backend.rebind_brightness(&entries);
                out.prepare_ms = out
                    .prepare_ms
                    .saturating_add(milliseconds(started.elapsed()));
                match rebound {
                    Ok(warnings) => out.warnings.extend(warnings),
                    Err(error) => {
                        // Retain every original brightness record; never run a
                        // batch against resources from before the HDR change.
                        out.warnings.push(error);
                        break;
                    }
                }
                if !current() {
                    out.superseded = true;
                    break;
                }
            }
            let group_started = out.elapsed_ms;
            let batch = Instant::now();
            let results = if let Some(hidden) = reveal.filter(|_| stage == 1 && hdr_confirmed) {
                let restored = self.backend.apply_restore(&changes, current, hidden);
                presentation = restored.presentation;
                restored.changes
            } else {
                self.backend.apply(&changes, current)
            };
            if stage == 0 {
                hdr_confirmed = results.len() == changes.len()
                    && results.iter().all(|r| matches!(r.result, Ok(true)));
            }
            for result in results {
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
        presentation
    }
}
fn merge_restore_attempts(mut first: Outcome, retry: Outcome) -> Outcome {
    let failed: Vec<_> = first
        .changes
        .iter()
        .filter_map(|c| c.result.as_ref().err().cloned())
        .collect();
    first.warnings.retain(|w| !failed.contains(w));
    first.warnings.extend(retry.warnings);
    first.superseded |= retry.superseded;
    first.presentation_ms = first.presentation_ms.or_else(|| {
        retry
            .presentation_ms
            .map(|ms| f64::from(first.total_ms) + ms)
    });
    for mut result in retry.changes {
        if let Some(previous) = first
            .changes
            .iter_mut()
            .find(|r| r.id == result.id && r.kind == result.kind)
        {
            result.elapsed_ms += previous.elapsed_ms;
            result.pre_read_ms += previous.pre_read_ms;
            result.write_ms += previous.write_ms;
            result.verify_ms += previous.verify_ms;
            result.verify_reads += previous.verify_reads;
            result.wait_ms += previous.wait_ms;
            result.attempts += previous.attempts;
            result.started_ms = previous.started_ms;
            *previous = result;
        } else {
            result.started_ms += f64::from(first.elapsed_ms);
            first.changes.push(result);
        }
    }
    first.channels = first
        .changes
        .iter()
        .filter(|r| matches!(r.result, Ok(true)))
        .count();
    first.elapsed_ms = first.elapsed_ms.saturating_add(retry.elapsed_ms);
    first.write_ms = first.write_ms.max(
        first
            .elapsed_ms
            .saturating_sub(retry.elapsed_ms)
            .saturating_add(retry.write_ms),
    );
    first.prepare_ms = first.prepare_ms.saturating_add(retry.prepare_ms);
    first.total_ms = first.total_ms.saturating_add(retry.total_ms);
    first
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
            atomic::{AtomicBool, AtomicUsize, Ordering},
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
    struct RequestedAudit {
        inner: Fake,
        store: Rc<MemoryStore>,
        requests: Vec<ScanRequest>,
        restores: Vec<(ScanRequest, bool)>,
    }
    impl Backend for RequestedAudit {
        fn scan(&mut self) -> Result<Inventory, String> {
            panic!("Engine must pass the requested hardware kinds before scanning")
        }
        fn scan_required(&mut self, request: ScanRequest) -> Result<Inventory, String> {
            self.requests.push(request);
            self.inner.scan_required(request)
        }
        fn prepare_restore(
            &mut self,
            entries: &[Device],
            allow_reuse: bool,
        ) -> Result<Vec<String>, String> {
            self.restores
                .push((ScanRequest::from_devices(entries), allow_reuse));
            self.inner.prepare_restore(entries, allow_reuse)
        }
        fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
            assert!(
                self.store
                    .saved
                    .borrow()
                    .as_ref()
                    .is_some_and(|j| j.entries.iter().any(|d| d.id == id && d.kind == kind)),
                "baseline must be durable before write"
            );
            self.inner.write(id, kind, value)
        }
        fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
            self.inner.read(id, kind)
        }
        fn cursor(&mut self, hidden: bool) -> Result<(), String> {
            self.inner.cursor(hidden)
        }
        fn remote_active(&self) -> bool {
            false
        }
    }
    #[test]
    fn measurement_commits_one_complete_baseline_and_restores_even_after_cancel() {
        for order in [DimOrder::BlackFirst, DimOrder::Fade, DimOrder::DimFirst] {
            let store = Rc::new(MemoryStore::default());
            let backend = RequestedAudit {
                inner: Fake::default(),
                store: store.clone(),
                requests: vec![],
                restores: vec![],
            };
            let mut engine = Engine::new(backend, store.clone()).unwrap();
            let original = Config {
                dim_order: order,
                cursor_mode: CursorMode::Hide,
                hdr_off: false,
                ..Config::default()
            };
            let cfg = measurement_config(&original);
            assert_eq!(original.dim_order, order);
            assert_eq!(original.cursor_mode, CursorMode::Hide);
            assert_eq!(cfg.dim_order, DimOrder::DimFirst);
            assert_eq!(cfg.cursor_mode, CursorMode::Keep);
            let live = Arc::new(AtomicBool::new(true));
            engine.backend.inner.cancel = Some(live.clone());
            let out = engine.dark(&cfg, || live.load(Ordering::Acquire)).unwrap();
            assert!(out.superseded);
            assert_eq!(store.saves.get(), 1);
            assert!(store
                .saved
                .borrow()
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .all(|d| d.kind == Kind::External));
            engine.backend.inner.cancel = None;
            engine.light_after_measure(|| true).unwrap();
            assert_eq!(engine.backend.inner.values["same-model/serial-b"], 76);
            assert_eq!(engine.backend.inner.values["hdr-a"], 1);
            assert!(!engine.has_pending_recovery());
            assert_eq!(
                engine.backend.requests,
                vec![ScanRequest {
                    external: true,
                    internal: true,
                    hdr: false
                }]
            );
            assert_eq!(
                engine.backend.restores,
                vec![(
                    ScanRequest {
                        external: true,
                        internal: false,
                        hdr: false
                    },
                    true
                )]
            );
        }
    }
    #[test]
    fn measurement_failed_full_baseline_never_writes_hardware() {
        let store = Rc::new(MemoryStore::default());
        store.fail.set(true);
        let mut engine = Engine::new(Fake::default(), store.clone()).unwrap();
        assert!(engine
            .dark(&measurement_config(&Config::default()), || true)
            .is_err());
        assert_eq!(store.saves.get(), 1);
        assert!(engine.backend.writes.is_empty());
        assert!(!engine.backend.cursor);
        assert!(!engine.has_pending_recovery());
    }
    #[test]
    fn restore_request_uses_original_journal_kinds_and_never_assumes_fast_reuse() {
        let store = Rc::new(MemoryStore::default());
        let backend = RequestedAudit {
            inner: Fake::default(),
            store: store.clone(),
            requests: vec![],
            restores: vec![],
        };
        let mut engine = Engine::new(backend, store).unwrap();
        let cfg = Config {
            dim_external: false,
            dim_internal: false,
            hdr_off: true,
            cursor_mode: CursorMode::Keep,
            ..Config::default()
        };
        engine.dark(&cfg, || true).unwrap();
        assert_eq!(
            engine.backend.requests,
            vec![ScanRequest {
                external: false,
                internal: false,
                hdr: true
            }]
        );
        assert_eq!(engine.backend.inner.values["same-model/serial-b"], 76);
        engine.light(|| true).unwrap();
        assert_eq!(
            engine.backend.restores,
            vec![(
                ScanRequest {
                    external: false,
                    internal: false,
                    hdr: true
                },
                false
            )]
        );
        assert_eq!(engine.backend.inner.values["hdr-a"], 1);
        assert!(!engine.has_pending_recovery());
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
    #[test]
    fn write_ack_requires_every_requested_change_and_fires_once() {
        let current = AtomicBool::new(true);
        let notified = AtomicUsize::new(0);
        let current_check = || current.load(Ordering::Acquire);
        let all_written = || {
            notified.fetch_add(1, Ordering::AcqRel);
        };
        let ack = WriteAcknowledgements::new(3, &current_check, &all_written);
        ack.success();
        ack.success();
        assert_eq!(notified.load(Ordering::Acquire), 0);
        ack.success();
        ack.success();
        assert_eq!(notified.load(Ordering::Acquire), 1);
        current.store(false, Ordering::Release);
        ack.success();
        assert_eq!(notified.load(Ordering::Acquire), 1);
    }
    #[test]
    fn write_ack_is_posted_before_confirmation_read() {
        let change = Change {
            device: Fake::default().scan().unwrap().devices.remove(0),
            value: 0,
        };
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let callback_events = events.clone();
        let written = || callback_events.lock().unwrap().push("ack");
        let mut state = 75;
        let result = confirm_change_after_snapshot_with_ack(
            &mut state,
            &change,
            &|| true,
            Instant::now(),
            |state| {
                events.lock().unwrap().push("read");
                Ok(*state)
            },
            |state, value| {
                events.lock().unwrap().push("write");
                *state = value;
                Ok(())
            },
            &written,
        );
        assert!(matches!(result.result, Ok(true)));
        assert_eq!(*events.lock().unwrap(), ["write", "ack", "read"]);
    }
}
