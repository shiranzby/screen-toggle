use crate::config::Config;
use std::collections::BTreeSet;

#[derive(Default)]
pub struct Schedule {
    date: u32,
    handled: BTreeSet<u16>,
    manual_ms: Option<u64>,
}
impl Schedule {
    pub fn manual(&mut self, monotonic_ms: u64) {
        self.manual_ms = Some(monotonic_ms);
    }
    pub fn tick(
        &mut self,
        date: u32,
        minute: u16,
        monotonic_ms: u64,
        cfg: &Config,
    ) -> Option<bool> {
        if date != self.date {
            self.date = date;
            self.handled.clear();
        }
        if !cfg.schedule_enabled
            || (minute != cfg.dark_at.minutes() && minute != cfg.light_at.minutes())
            || !self.handled.insert(minute)
        {
            return None;
        }
        if self
            .manual_ms
            .is_some_and(|t| monotonic_ms.saturating_sub(t) < 600_000)
        {
            return None;
        }
        Some(minute == cfg.dark_at.minutes())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_dst_and_manual_override() {
        let c = Config {
            schedule_enabled: true,
            ..Config::default()
        };
        let mut s = Schedule::default();
        assert_eq!(s.tick(20261101, 1380, 0, &c), Some(true));
        assert_eq!(s.tick(20261101, 1380, 1000, &c), None);
        assert_eq!(s.tick(20261101, 1300, 2000, &c), None);
        assert_eq!(s.tick(20261101, 1380, 3000, &c), None);
        assert_eq!(s.tick(20261102, 1380, 86_400_000, &c), Some(true));
        s.manual(86_400_000);
        assert_eq!(s.tick(20261103, 420, 86_400_010, &c), None);
        assert_eq!(s.tick(20261103, 420, 87_100_000, &c), None);
        assert_eq!(s.tick(20261104, 420, 172_800_000, &c), Some(false));
    }
}
