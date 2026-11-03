use sog::{
    config::{display_is_selected, Config, CursorMode, DimOrder},
    engine::{Backend, Device, Engine, Inventory, Journal, JournalStore, Kind},
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

const A: &str = r"\\?\DISPLAY#ACM1000#5&abc&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
const B: &str = r"\\?\DISPLAY#ACM1000#5&abc&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
const INTERNAL: &str = r"\\?\DISPLAY#BOE1000#4&def&0&UID256#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";

#[test]
fn configuration_preserves_legacy_all_and_explicit_none() {
    assert_eq!(Config::decode("{}").unwrap().selected_displays, None);
    let empty = Config::decode(r#"{"selected_displays":[]}"#).unwrap();
    assert_eq!(empty.selected_displays, Some(Vec::new()));
    let cfg = Config {
        selected_displays: Some(vec![A.into(), INTERNAL.into()]),
        ..Config::default()
    };
    assert_eq!(
        Config::decode(&serde_json::to_string(&cfg).unwrap()).unwrap(),
        cfg
    );
    for ids in [
        vec![A.into(), A.to_ascii_lowercase()],
        vec![String::new()],
        vec!["DISPLAY1".into()],
        vec!["ddc:screen".into()],
        vec![format!(" {A}")],
        vec![A.replace("UID4352", "UID4352\n")],
        vec![A.into(); 257],
    ] {
        assert!(Config {
            selected_displays: Some(ids),
            ..Config::default()
        }
        .validate()
        .is_err());
    }
}

#[test]
fn mapping_uses_complete_pnp_identity_and_exact_wmi_suffix() {
    let selected = [A.to_ascii_lowercase(), INTERNAL.into()];
    for id in [format!("ddc:{A}"), format!("hdr:{A}"), A.into()] {
        assert!(display_is_selected(&id, Some(&selected)));
    }
    assert!(display_is_selected(
        r"wmi:DISPLAY\BOE1000\4&def&0&UID256_0",
        Some(&selected)
    ));
    for id in [
        format!("ddc:{B}"),
        format!("hdr:{B}"),
        "wmi:DISPLAY\\BOE1000\\4&def&0&UID256_1".into(),
        "wmi:DISPLAY\\BOE1000\\4&def&0&UID2560_0".into(),
        "wmi:DISPLAY\\BOE100\\4&def&0&UID256_0".into(),
        "wmi:DISPLAY\\BOE1000\\4&def&0&UID256".into(),
        "DISPLAY1".into(),
    ] {
        assert!(!display_is_selected(&id, Some(&selected)), "{id}");
    }
    assert!(!display_is_selected(&format!("ddc:{A}"), Some(&[])));
    assert!(display_is_selected("legacy-simulated-device", None));
}

#[derive(Clone, Default)]
struct Memory(Rc<RefCell<Option<Journal>>>);
impl JournalStore for Memory {
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
struct Simulated {
    devices: BTreeMap<String, Device>,
    writes: Vec<(String, u32)>,
}
impl Simulated {
    fn new() -> Self {
        let devices = [
            (format!("ddc:{A}"), Kind::External, 73),
            (format!("ddc:{B}"), Kind::External, 61),
            (
                "wmi:DISPLAY\\BOE1000\\4&def&0&UID256_0".into(),
                Kind::Internal,
                64,
            ),
        ]
        .into_iter()
        .map(|(id, kind, value)| {
            (
                id.clone(),
                Device {
                    name: id.clone(),
                    id,
                    kind,
                    min: 0,
                    value,
                    max: 100,
                },
            )
        })
        .collect();
        Self {
            devices,
            writes: Vec::new(),
        }
    }
}
impl Backend for Simulated {
    fn scan(&mut self) -> Result<Inventory, String> {
        Ok(Inventory {
            devices: self.devices.values().cloned().collect(),
            warnings: Vec::new(),
        })
    }
    fn write(&mut self, id: &str, _kind: Kind, value: u32) -> Result<(), String> {
        self.devices.get_mut(id).ok_or("missing display")?.value = value;
        self.writes.push((id.into(), value));
        Ok(())
    }
    fn read(&mut self, id: &str, _kind: Kind) -> Result<u32, String> {
        Ok(self.devices.get(id).ok_or("missing display")?.value)
    }
    fn cursor(&mut self, _hidden: bool) -> Result<(), String> {
        Ok(())
    }
    fn remote_active(&self) -> bool {
        false
    }
}
fn selected(ids: Option<Vec<String>>) -> Config {
    Config {
        selected_displays: ids,
        cursor_mode: CursorMode::Keep,
        dim_order: DimOrder::DimFirst,
        ..Config::default()
    }
}

#[test]
fn missing_or_empty_selection_never_falls_back_to_other_displays() {
    let missing = A.replace("UID4352", "UID9999");
    for ids in [Some(Vec::new()), Some(vec![missing])] {
        let mut engine = Engine::new(Simulated::new(), Memory::default()).unwrap();
        let out = engine.dark(&selected(ids), || true).unwrap();
        assert_eq!(out.channels, 0);
        assert!(engine.backend.writes.is_empty());
        engine.light(|| true).unwrap();
        assert!(!engine.has_pending_recovery());
    }
}

#[test]
fn changed_selection_restores_original_journal_before_dimming_new_screen() {
    let store = Memory::default();
    let mut engine = Engine::new(Simulated::new(), store.clone()).unwrap();
    engine
        .dark(&selected(Some(vec![A.into()])), || true)
        .unwrap();
    assert_eq!(store.0.borrow().as_ref().unwrap().entries.len(), 1);
    assert_eq!(store.0.borrow().as_ref().unwrap().entries[0].value, 73);
    // Changing the configured selection cannot filter out A's pending recovery.
    engine
        .dark(&selected(Some(vec![B.into()])), || true)
        .unwrap();
    assert_eq!(engine.backend.devices[&format!("ddc:{A}")].value, 73);
    assert_eq!(
        store.0.borrow().as_ref().unwrap().entries[0].id,
        format!("ddc:{B}")
    );
    engine.light(|| true).unwrap();
    assert_eq!(
        engine.backend.writes,
        vec![
            (format!("ddc:{A}"), 0),
            (format!("ddc:{A}"), 73),
            (format!("ddc:{B}"), 0),
            (format!("ddc:{B}"), 61),
        ]
    );
    assert!(!engine.has_pending_recovery());
}

#[test]
fn old_configuration_keeps_all_brightness_channels() {
    let mut engine = Engine::new(Simulated::new(), Memory::default()).unwrap();
    assert_eq!(engine.dark(&selected(None), || true).unwrap().channels, 3);
    engine.light(|| true).unwrap();
    assert_eq!(engine.backend.writes.len(), 6);
    assert!(!engine.has_pending_recovery());
}
