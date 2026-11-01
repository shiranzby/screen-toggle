use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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
    pub fn validate(self) -> Result<(), String> {
        let key_ok =
            matches!(self.key, 0x30..=0x39 | 0x41..=0x5A | 0x70..=0x87 | 0x20..=0x28 | 0x2D..=0x2E);
        if !key_ok
            || self.modifiers & !15 != 0
            || (self.modifiers & (1 | 2 | 8) == 0 && !matches!(self.key, 0x70..=0x87))
        {
            return Err("请选择 Ctrl / Alt 加字母、数字等组合键，或 F1–F24".into());
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
    pub cursor_mode: CursorMode,
    pub hdr_off: bool,
    pub close_action: CloseAction,
    pub autostart: bool,
    pub hotkey: Hotkey,
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
            cursor_mode: CursorMode::Auto,
            hdr_off: false,
            close_action: CloseAction::Hide,
            autostart: false,
            hotkey: Hotkey::default(),
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
        for t in [self.dark_at, self.light_at] {
            if t.hour > 23 || t.minute > 59 {
                return Err("定时时间无效".into());
            }
        }
        if self.dark_at == self.light_at {
            return Err("熄屏与亮屏时间不能相同".into());
        }
        self.hotkey.validate()
    }
    pub fn decode(text: &str) -> Result<Self, String> {
        let mut cfg: Self = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("配置读取失败：{e}"))?;
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
