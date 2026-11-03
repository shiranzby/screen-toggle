use sog::{
    config::{Config, CursorMode},
    engine::{Backend, Device, Engine, Inventory, Journal, JournalStore, Kind},
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Clone, Default)]
struct Store(Rc<RefCell<Option<Journal>>>);
impl JournalStore for Store {
    fn load(&self) -> Result<Option<Journal>, String> {
        Ok(self.0.borrow().clone())
    }
    fn save(&self, journal: &Journal) -> Result<(), String> {
        *self.0.borrow_mut() = Some(journal.clone());
        Ok(())
    }
    fn clear(&self) -> Result<(), String> {
        *self.0.borrow_mut() = None;
        Ok(())
    }
}

struct HdrDisplay {
    values: BTreeMap<String, u32>,
    brightness_bound: bool,
    require_rebind: bool,
    fail_hdr_restore: bool,
    fail_hdr_once: bool,
    preparations: Vec<Vec<Kind>>,
}
impl HdrDisplay {
    fn new(require_rebind: bool) -> Self {
        Self {
            values: BTreeMap::from([("hdr:display".into(), 1), ("ddc:display".into(), 73)]),
            brightness_bound: true,
            require_rebind,
            fail_hdr_restore: false,
            fail_hdr_once: false,
            preparations: vec![],
        }
    }
}
impl Backend for HdrDisplay {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.brightness_bound = true;
        Ok(Inventory {
            devices: self
                .values
                .iter()
                .map(|(id, &value)| Device {
                    id: id.clone(),
                    name: id.clone(),
                    kind: if id.starts_with("hdr:") {
                        Kind::Hdr
                    } else {
                        Kind::External
                    },
                    min: 0,
                    max: if id.starts_with("hdr:") { 1 } else { 100 },
                    value,
                })
                .collect(),
            warnings: vec![],
        })
    }
    fn prepare_restore(&mut self, entries: &[Device], _: bool) -> Result<Vec<String>, String> {
        self.preparations
            .push(entries.iter().map(|d| d.kind).collect());
        if entries.iter().any(|d| d.kind == Kind::External) {
            self.brightness_bound = true;
        }
        Ok(vec![])
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        if kind == Kind::Hdr {
            if value == 1 && self.fail_hdr_once {
                self.fail_hdr_once = false;
                return Err("transient HDR restoration failure".into());
            }
            if value == 1 && self.fail_hdr_restore {
                return Err("transient HDR restoration failure".into());
            }
            // A mode switch can reset brightness and invalidate its handles.
            self.values.insert("ddc:display".into(), 0);
            self.brightness_bound = false;
        } else if self.require_rebind && !self.brightness_bound {
            return Err("physical handle invalidated by HDR".into());
        }
        self.values.insert(id.into(), value);
        Ok(())
    }
    fn read(&mut self, id: &str, _: Kind) -> Result<u32, String> {
        self.values.get(id).copied().ok_or("device absent".into())
    }
    fn cursor(&mut self, _: bool) -> Result<(), String> {
        Ok(())
    }
    fn remote_active(&self) -> bool {
        false
    }
}

fn config() -> Config {
    Config {
        hdr_off: true,
        cursor_mode: CursorMode::Keep,
        ..Config::default()
    }
}

#[test]
fn hdr_mode_change_rebinds_brightness_and_preserves_original_baseline() {
    let store = Store::default();
    let mut engine = Engine::new(HdrDisplay::new(true), store.clone()).unwrap();
    let dark = engine.dark(&config(), || true).unwrap();
    assert!(dark.warnings.is_empty(), "{:?}", dark.warnings);
    assert_eq!(dark.channels, 2);
    assert_eq!(
        store
            .0
            .borrow()
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .find(|d| d.kind == Kind::External)
            .unwrap()
            .value,
        73
    );
    let light = engine.light(|| true).unwrap();
    assert!(light.warnings.is_empty(), "{:?}", light.warnings);
    assert_eq!(engine.backend.values["ddc:display"], 73);
    assert_eq!(engine.backend.values["hdr:display"], 1);
    assert!(!engine.has_pending_recovery());
    assert_eq!(
        engine.backend.preparations,
        [vec![Kind::External], vec![Kind::Hdr], vec![Kind::External]]
    );
}

#[test]
fn failed_hdr_restore_keeps_brightness_baseline_for_dependent_retry() {
    let store = Store::default();
    let mut engine = Engine::new(HdrDisplay::new(false), store.clone()).unwrap();
    engine.dark(&config(), || true).unwrap();
    engine.backend.fail_hdr_restore = true;
    let failed = engine.light(|| true).unwrap();
    assert!(!failed.warnings.is_empty());
    let journal = store.0.borrow().clone().unwrap();
    assert!(
        journal
            .entries
            .iter()
            .any(|d| d.kind == Kind::External && d.value == 73),
        "HDR retry can reset a successfully restored brightness; keep its baseline"
    );
    engine.backend.fail_hdr_restore = false;
    engine.light(|| true).unwrap();
    assert_eq!(engine.backend.values["ddc:display"], 73);
    assert_eq!(engine.backend.values["hdr:display"], 1);
    assert!(!engine.has_pending_recovery());
}

#[test]
fn transient_hdr_failure_rebinds_and_restores_in_the_same_light_request() {
    let store = Store::default();
    let mut engine = Engine::new(HdrDisplay::new(true), store).unwrap();
    engine.dark(&config(), || true).unwrap();
    engine.backend.fail_hdr_once = true;
    let out = engine.light(|| true).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.channels, 2);
    assert!(out.changes.iter().all(|c| c.attempts == 2));
    assert_eq!(engine.backend.values["ddc:display"], 73);
    assert!(!engine.has_pending_recovery());
}

#[test]
fn no_hdr_restore_keeps_one_prepare_and_no_extra_attempt() {
    let store = Store::default();
    let mut engine = Engine::new(HdrDisplay::new(false), store).unwrap();
    let cfg = Config {
        hdr_off: false,
        ..config()
    };
    engine.dark(&cfg, || true).unwrap();
    engine.backend.preparations.clear();
    let out = engine.light(|| true).unwrap();
    assert!(out.warnings.is_empty());
    assert_eq!(engine.backend.preparations, [vec![Kind::External]]);
    assert!(out.changes.iter().all(|c| c.attempts == 1));
}
