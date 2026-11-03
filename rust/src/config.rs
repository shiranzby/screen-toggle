use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MONITOR_INTERFACE: &str = "{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";

/// Stable PnP identity shared by QueryDisplayConfig and the WMI brightness
/// provider. Only the documented monitor-interface shape is accepted; source
/// numbers, friendly names and partial instance strings cannot select a screen.
fn monitor_identity(id: &str) -> Option<String> {
    if id.len() > 1024 || id.trim() != id || id.chars().any(char::is_control) {
        return None;
    }
    let lower = id.to_ascii_lowercase();
    let path = lower.strip_prefix(r"\\?\")?;
    let parts: Vec<_> = path.split('#').collect();
    if parts.len() != 4
        || parts[0] != "display"
        || parts[1].is_empty()
        || parts[2].is_empty()
        || parts[3] != MONITOR_INTERFACE
        || parts[1..3]
            .iter()
            .any(|part| part.contains(['\\', '/', ':']) || part.trim() != *part)
    {
        return None;
    }
    Some(format!("display\\{}\\{}", parts[1], parts[2]))
}

/// The brightness WMI provider appends its instance suffix to the PnP identity.
/// Accept exactly `_0`, not a substring or an arbitrary trailing number.
fn wmi_display_identity(id: &str) -> Option<String> {
    let lower = id.to_ascii_lowercase();
    let parts: Vec<_> = lower.split('\\').collect();
    if parts.len() != 3 || parts[0] != "display" || parts[1].is_empty() {
        return None;
    }
    let instance = parts[2].strip_suffix("_0")?;
    if instance.is_empty() || parts[1..].iter().any(|p| p.contains(['#', '/', ':'])) {
        return None;
    }
    Some(format!("display\\{}\\{}", parts[1], instance))
}

pub fn valid_display_id(id: &str) -> bool {
    monitor_identity(id).is_some()
}

/// None preserves the historical all-display behavior. An empty list selects
/// nothing, including when a previously selected screen has disconnected.
pub fn display_is_selected(device_id: &str, selected: Option<&[String]>) -> bool {
    let Some(selected) = selected else {
        return true;
    };
    let lower = device_id.to_ascii_lowercase();
    let identity = if let Some(id) = lower
        .strip_prefix("ddc:")
        .or_else(|| lower.strip_prefix("hdr:"))
    {
        monitor_identity(id)
    } else if let Some(id) = lower.strip_prefix("wmi:") {
        wmi_display_identity(id)
    } else {
        monitor_identity(&lower)
    };
    identity.is_some_and(|identity| {
        selected
            .iter()
            .any(|id| monitor_identity(id).as_ref() == Some(&identity))
    })
}

pub(crate) fn display_wmi_instance(id: &str) -> Option<String> {
    monitor_identity(id).map(|identity| format!("{identity}_0"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Light,
    Dark,
    #[default]
    System,
}
impl Theme {
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::Light => false,
            Self::Dark => true,
            Self::System => system_dark,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimOrder {
    #[default]
    BlackFirst,
    DimFirst,
    Fade,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    #[default]
    Ready,
    Delay,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    #[default]
    Auto,
    Hide,
    Keep,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    #[default]
    Hide,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    pub modifiers: u32,
    pub key: u32,
}
impl Default for Hotkey {
    fn default() -> Self {
        Self {
            modifiers: 3,
            key: 0x53,
        }
    }
}
impl Hotkey {
    pub fn is_mouse(self) -> bool {
        matches!(self.key, 0x01 | 0x02 | 0x04 | 0x05 | 0x06)
    }
    pub fn validate(self) -> Result<(), String> {
        let key_ok = self.is_mouse()
            || matches!(self.key, 0x30..=0x39 | 0x41..=0x5A | 0x70..=0x87 | 0x20..=0x28 | 0x2D..=0x2E);
        if !key_ok
            || self.modifiers & !15 != 0
            || (self.modifiers & (1 | 2 | 8) == 0
                && !matches!(self.key, 0x70..=0x87)
                && !self.is_mouse())
        {
            return Err("请选择键盘组合键、F1–F24，或鼠标左/右/中键与侧键".into());
        }
        Ok(())
    }
    pub fn label(self) -> String {
        let mut parts: Vec<String> = [(2, "Ctrl"), (1, "Alt"), (4, "Shift"), (8, "Win")]
            .into_iter()
            .filter(|(bit, _)| self.modifiers & bit != 0)
            .map(|(_, s)| s.into())
            .collect();
        parts.push(match self.key {
            0x01 => "鼠标左键".into(),
            0x02 => "鼠标右键".into(),
            0x04 => "鼠标中键（滚轮按下）".into(),
            0x05 => "鼠标侧键 1".into(),
            0x06 => "鼠标侧键 2".into(),
            0x30..=0x39 | 0x41..=0x5A => char::from_u32(self.key).unwrap_or('?').to_string(),
            0x70..=0x87 => format!("F{}", self.key - 0x6f),
            0x20 => "Space".into(),
            0x21 => "PageUp".into(),
            0x22 => "PageDown".into(),
            0x23 => "End".into(),
            0x24 => "Home".into(),
            0x25 => "Left".into(),
            0x26 => "Up".into(),
            0x27 => "Right".into(),
            0x28 => "Down".into(),
            0x2d => "Insert".into(),
            0x2e => "Delete".into(),
            _ => format!("0x{:02X}", self.key),
        });
        parts.join(" + ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Time {
    pub hour: u8,
    pub minute: u8,
}
impl Time {
    pub fn minutes(self) -> u16 {
        u16::from(self.hour) * 60 + u16::from(self.minute)
    }
    pub fn label(self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        let normalized = s.trim().replace('：', ":");
        let (h, m) = if let Some((h, m)) = normalized.split_once(':') {
            (h.trim(), m.trim())
        } else if normalized.len() == 4 && normalized.bytes().all(|c| c.is_ascii_digit()) {
            (&normalized[..2], &normalized[2..])
        } else {
            return Err("请输入有效时间，例如 23:30 或 2330".into());
        };
        if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|c| c.is_ascii_digit()) {
            return Err("请输入完整的两位小时与分钟，例如 23:30".into());
        }
        let hour = h.parse::<u8>().map_err(|_| "小时无效")?;
        let minute = m.parse::<u8>().map_err(|_| "分钟无效")?;
        if hour > 23 || minute > 59 {
            return Err("时间范围为 00:00–23:59".into());
        }
        Ok(Self { hour, minute })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub theme: Theme,
    pub dim_order: DimOrder,
    pub restore_mode: RestoreMode,
    pub delay_ms: u32,
    pub dim_internal: bool,
    pub dim_external: bool,
    pub selected_displays: Option<Vec<String>>,
    pub cursor_mode: CursorMode,
    pub hdr_off: bool,
    pub close_action: CloseAction,
    pub autostart: bool,
    pub hotkey: Hotkey,
    pub hotkey_double_click: bool,
    pub desktop_trigger_enabled: bool,
    pub desktop_hotkey: Hotkey,
    pub desktop_double_click: bool,
    pub schedule_enabled: bool,
    pub dark_at: Time,
    pub light_at: Time,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            theme: Theme::System,
            dim_order: DimOrder::BlackFirst,
            restore_mode: RestoreMode::Ready,
            delay_ms: 140,
            dim_internal: true,
            dim_external: true,
            selected_displays: None,
            cursor_mode: CursorMode::Auto,
            hdr_off: false,
            close_action: CloseAction::Hide,
            autostart: false,
            hotkey: Hotkey::default(),
            hotkey_double_click: false,
            desktop_trigger_enabled: false,
            desktop_hotkey: Hotkey {
                modifiers: 0,
                key: 0x05,
            },
            desktop_double_click: true,
            schedule_enabled: false,
            dark_at: Time {
                hour: 23,
                minute: 0,
            },
            light_at: Time { hour: 7, minute: 0 },
            extra: BTreeMap::new(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("配置版本不受支持，原文件已保留".into());
        }
        if self.delay_ms > 500 {
            return Err("亮屏延迟范围为 0–500 ms".into());
        }
        if let Some(selected) = &self.selected_displays {
            let mut identities = BTreeSet::new();
            if selected.len() > 256
                || selected
                    .iter()
                    .any(|id| !valid_display_id(id) || !identities.insert(id.to_ascii_lowercase()))
            {
                return Err("请选择有效且不重复的完整显示器身份（最多 256 屏）".into());
            }
        }
        for t in [self.dark_at, self.light_at] {
            if t.hour > 23 || t.minute > 59 {
                return Err("定时时间无效".into());
            }
        }
        if self.dark_at == self.light_at {
            return Err("熄屏与亮屏时间不能相同".into());
        }
        self.hotkey.validate()?;
        self.desktop_hotkey.validate()?;
        if self.desktop_trigger_enabled && self.hotkey == self.desktop_hotkey {
            return Err("全局与桌面触发热键不能相同，请修改其中一组".into());
        }
        Ok(())
    }
    pub fn decode(text: &str) -> Result<Self, String> {
        let mut value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("配置读取失败：{e}"))?;
        // An unpublished candidate used the double-click flag as its enable
        // preference and stored a mouse-only choice. Preserve that intent once.
        if let Some(object) = value.as_object_mut() {
            if let Some(button) = object.remove("desktop_mouse_button") {
                let key = match button.as_str() {
                    Some("left") => 0x01,
                    Some("middle") => 0x04,
                    Some("x1") => 0x05,
                    Some("x2") => 0x06,
                    _ => return Err("旧桌面鼠标按键无效，原配置已保留".into()),
                };
                let enabled = object
                    .get("desktop_double_click")
                    .cloned()
                    .unwrap_or(Value::Bool(false));
                object.entry("desktop_trigger_enabled").or_insert(enabled);
                object
                    .entry("desktop_hotkey")
                    .or_insert(serde_json::json!({"modifiers":0,"key":key}));
                object.insert("desktop_double_click".into(), Value::Bool(true));
            }
        }
        let mut cfg: Self =
            serde_json::from_value(value).map_err(|e| format!("配置读取失败：{e}"))?;
        // Silent launch belongs to the startup command, never manual launch.
        cfg.extra.remove("silent_start");
        cfg.validate()?;
        Ok(cfg)
    }
    /// Read either historical Rust sog.json or Python settings.json, without writing it.
    pub fn migrate(text: &str) -> Result<Self, String> {
        let v: Value =
            serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
        let obj = v.as_object().ok_or("旧配置不是 JSON 对象")?;
        let mut c = Self::default();
        let boolean = |key: &str, fallback: bool| -> Result<bool, String> {
            match obj.get(key) {
                None => Ok(fallback),
                Some(Value::Bool(b)) => Ok(*b),
                Some(Value::Number(n)) if n.as_u64() == Some(0) => Ok(false),
                Some(Value::Number(n)) if n.as_u64() == Some(1) => Ok(true),
                _ => Err(format!("旧配置字段 {key} 无效")),
            }
        };
        let number = |key: &str, fallback: u32| -> Result<u32, String> {
            match obj.get(key) {
                None => Ok(fallback),
                Some(v) => v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| format!("旧配置字段 {key} 无效")),
            }
        };
        c.dim_external = boolean("dim_external", c.dim_external)?;
        c.dim_internal = boolean("dim_internal", c.dim_internal)?;
        c.hdr_off = boolean(
            if obj.contains_key("hdr_off") {
                "hdr_off"
            } else {
                "hdr_off_when_dark"
            },
            c.hdr_off,
        )?;
        c.autostart = boolean("autostart", false)?;
        c.schedule_enabled = boolean("sched_enabled", false)?;
        c.delay_ms = number(
            if obj.contains_key("delay_ms") {
                "delay_ms"
            } else {
                "restore_delay_ms"
            },
            140,
        )?;
        c.hotkey = Hotkey {
            modifiers: number(
                if obj.contains_key("hotkey_mod") {
                    "hotkey_mod"
                } else {
                    "hotkey_mods"
                },
                3,
            )?,
            key: number("hotkey_vk", 0x53)?,
        };
        c.dim_order = match obj.get("dim_order") {
            None => DimOrder::BlackFirst,
            Some(Value::Number(n)) => match n.as_u64() {
                Some(0) => DimOrder::BlackFirst,
                Some(1) => DimOrder::DimFirst,
                Some(2) => DimOrder::Fade,
                _ => return Err("旧熄屏顺序无效".into()),
            },
            Some(Value::String(s)) => match s.as_str() {
                "black_first" => DimOrder::BlackFirst,
                "dim_first" => DimOrder::DimFirst,
                "fade" => DimOrder::Fade,
                _ => return Err("旧熄屏顺序无效".into()),
            },
            _ => return Err("旧熄屏顺序无效".into()),
        };
        c.restore_mode = match obj.get("restore_mode") {
            None => RestoreMode::Ready,
            Some(Value::Number(n)) if n.as_u64() == Some(0) => RestoreMode::Ready,
            Some(Value::Number(n)) if n.as_u64() == Some(1) => RestoreMode::Delay,
            Some(Value::String(s)) if s == "ready" => RestoreMode::Ready,
            Some(Value::String(s)) if s == "delay" => RestoreMode::Delay,
            _ => return Err("旧亮屏方式无效".into()),
        };
        c.cursor_mode = match obj.get("cursor_mode") {
            None => CursorMode::Auto,
            Some(Value::Number(n)) => match n.as_u64() {
                Some(0) => CursorMode::Auto,
                Some(1) => CursorMode::Hide,
                Some(2) => CursorMode::Keep,
                _ => return Err("旧光标策略无效".into()),
            },
            Some(Value::String(s)) => match s.as_str() {
                "auto" => CursorMode::Auto,
                "hide" => CursorMode::Hide,
                "keep" => CursorMode::Keep,
                _ => return Err("旧光标策略无效".into()),
            },
            _ => return Err("旧光标策略无效".into()),
        };
        c.close_action = match obj.get("close_action") {
            None | Some(Value::Null) => CloseAction::Hide,
            Some(Value::Number(n)) if n.as_u64() == Some(0) => CloseAction::Hide,
            Some(Value::Number(n)) if n.as_u64() == Some(1) => CloseAction::Exit,
            Some(Value::String(s)) if s == "hide" => CloseAction::Hide,
            Some(Value::String(s)) if s == "exit" => CloseAction::Exit,
            _ => return Err("旧关闭方式无效".into()),
        };
        c.dark_at = Time {
            hour: u8::try_from(number("sched_on_h", 23)?).map_err(|_| "旧小时溢出")?,
            minute: u8::try_from(number("sched_on_m", 0)?).map_err(|_| "旧分钟溢出")?,
        };
        c.light_at = Time {
            hour: u8::try_from(number("sched_off_h", 7)?).map_err(|_| "旧小时溢出")?,
            minute: u8::try_from(number("sched_off_m", 0)?).map_err(|_| "旧分钟溢出")?,
        };
        c.validate()?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_trigger_preferences_and_all_mouse_names_persist() {
        let old = Config::decode("{}").unwrap();
        assert!(!old.desktop_trigger_enabled);
        assert!(!old.hotkey_double_click);
        assert!(old.desktop_double_click);
        assert_eq!(
            old.desktop_hotkey,
            Hotkey {
                modifiers: 0,
                key: 5
            }
        );
        for (key, label) in [
            (1, "鼠标左键"),
            (2, "鼠标右键"),
            (4, "鼠标中键（滚轮按下）"),
            (5, "鼠标侧键 1"),
            (6, "鼠标侧键 2"),
        ] {
            let cfg = Config {
                desktop_trigger_enabled: true,
                desktop_hotkey: Hotkey { modifiers: 0, key },
                desktop_double_click: true,
                hotkey_double_click: false,
                ..old.clone()
            };
            let restored = Config::decode(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert!(restored.desktop_trigger_enabled);
            assert!(restored.desktop_double_click);
            assert!(!restored.hotkey_double_click);
            assert!(restored.desktop_hotkey.is_mouse());
            assert_eq!(restored.desktop_hotkey.label(), label);
            assert_eq!(restored.hotkey, old.hotkey);
        }
    }
    #[test]
    fn candidate_mouse_only_preferences_migrate_once_without_losing_enable_state() {
        for enabled in [false, true] {
            let cfg = Config::decode(&format!(
                "{{\"desktop_double_click\":{enabled},\"desktop_mouse_button\":\"x2\",\"unknown\":9}}"
            )).unwrap();
            assert_eq!(cfg.desktop_trigger_enabled, enabled);
            assert!(cfg.desktop_double_click);
            assert_eq!(
                cfg.desktop_hotkey,
                Hotkey {
                    modifiers: 0,
                    key: 6
                }
            );
            let stored = serde_json::to_value(&cfg).unwrap();
            assert!(stored.get("desktop_mouse_button").is_none());
            assert_eq!(stored["unknown"], 9);
        }
        assert!(Config::decode(r#"{"desktop_mouse_button":"invalid"}"#).is_err());
    }
    #[test]
    fn overlapping_enabled_triggers_are_rejected_and_mouse_modifiers_are_valid() {
        let mouse = Hotkey {
            modifiers: 2,
            key: 5,
        };
        assert!(mouse.validate().is_ok());
        assert_eq!(mouse.label(), "Ctrl + 鼠标侧键 1");
        let cfg = Config {
            hotkey: mouse,
            desktop_hotkey: mouse,
            desktop_trigger_enabled: true,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
        assert!(Config {
            desktop_trigger_enabled: false,
            ..cfg
        }
        .validate()
        .is_ok());
        assert!(Hotkey {
            modifiers: 0,
            key: 0x41
        }
        .validate()
        .is_err());
        assert!(Hotkey {
            modifiers: 16,
            key: 5
        }
        .validate()
        .is_err());
    }
    #[test]
    fn retired_silent_preference_does_not_change_autostart_or_survive_save() {
        for autostart in [false, true] {
            for silent in [false, true] {
                let cfg = Config::decode(&format!(
                    "{{\"autostart\":{autostart},\"silent_start\":{silent},\"keep_me\":7}}"
                ))
                .unwrap();
                assert_eq!(cfg.autostart, autostart);
                let stored = serde_json::to_value(&cfg).unwrap();
                assert!(stored.get("silent_start").is_none());
                assert_eq!(stored["keep_me"], 7);
            }
        }
    }
    #[test]
    fn theme_is_compatible_with_existing_config_and_explicit_choices_persist() {
        let old = Config::decode("{\"schema_version\":1,\"extra_preference\":42}").unwrap();
        assert_eq!(old.theme, Theme::System);
        for theme in [Theme::Light, Theme::Dark, Theme::System] {
            let mut cfg = old.clone();
            cfg.theme = theme;
            let decoded = Config::decode(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert_eq!(decoded.theme, theme);
            assert_eq!(decoded.extra["extra_preference"], 42);
        }
        assert!(!Theme::Light.is_dark(true));
        assert!(Theme::Dark.is_dark(false));
        assert!(!Theme::System.is_dark(false));
        assert!(Theme::System.is_dark(true));
        assert!(Config::decode("{\"theme\":\"invalid\"}").is_err());
    }
    #[test]
    fn migration_keeps_real_preferences() {
        let c=Config::migrate(r#"{"dim_order":2,"restore_mode":1,"dim_internal":0,"dim_external":1,"hotkey_mod":6,"hotkey_vk":75,"delay_ms":126,"autostart":1,"cursor_mode":2,"sched_on_h":3,"sched_on_m":36}"#).unwrap();
        assert_eq!(c.dim_order, DimOrder::Fade);
        assert!(!c.dim_internal);
        assert!(c.dim_external);
        assert_eq!(c.hotkey.label(), "Ctrl + Shift + K");
        assert_eq!(c.dark_at.label(), "03:36");
        assert_eq!(c.delay_ms, 126);
        assert_eq!(
            Config::decode(&serde_json::to_string(&c).unwrap()).unwrap(),
            c
        );
    }
    #[test]
    fn reject_overflow_and_conflicting_schedule() {
        assert!(Config::migrate(r#"{"sched_on_h":256}"#).is_err());
        assert!(Config::migrate(r#"{"dim_order":256}"#).is_err());
        let mut c = Config::default();
        c.dark_at = c.light_at;
        assert!(c.validate().is_err());
        assert!(Config::decode(r#"{"schema_version":2}"#).is_err());
    }
    #[test]
    fn editing_times_accepts_chinese_and_rejects_partial_input() {
        for s in ["23:30", "2330", " 23：30 "] {
            assert_eq!(
                Time::parse(s).unwrap(),
                Time {
                    hour: 23,
                    minute: 30
                }
            );
        }
        for s in ["23:", "24:00", "10:60", "😀12", "2", "12:00:01"] {
            assert!(Time::parse(s).is_err(), "{s}");
        }
    }
    #[test]
    fn unknown_fields_survive_roundtrip() {
        let c = Config::decode(r#"{"future_preference":{"name":"原文"}}"#).unwrap();
        assert_eq!(
            Config::decode(&serde_json::to_string(&c).unwrap())
                .unwrap()
                .extra["future_preference"]["name"],
            "原文"
        );
    }
}
