/// Keep five seconds after any activity or uncertainty. This operates on original
/// replay time; changing output FPS or playback speed cannot change the threshold.
#[derive(Default)]
pub(crate) struct Inactivity {
    last_active_ms: f64,
    tail: Option<u64>,
}

pub(crate) struct Decision {
    pub keep: bool,
    pub tail: Option<u64>,
}

impl Inactivity {
    pub fn select(&mut self, index: u64, time_ms: f64, active: bool, terminal: bool) -> Decision {
        if active {
            self.last_active_ms = time_ms;
        }
        let keep = index == 0 || active || terminal || time_ms - self.last_active_ms <= 5000.0;
        let tail = if keep {
            self.tail.take()
        } else {
            self.tail = Some(index);
            None
        };
        Decision { keep, tail }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_middle_is_removed_but_boundary_states_and_activity_are_retained() {
        let mut filter = Inactivity::default();
        let mut kept = Vec::new();
        for i in 0..=40 {
            let d = filter.select(i, i as f64 * 1000.0, i == 0 || i == 30, i == 40);
            kept.extend(d.tail);
            if d.keep {
                kept.push(i);
            }
        }
        assert_eq!(kept, [0, 1, 2, 3, 4, 5, 29, 30, 31, 32, 33, 34, 35, 39, 40]);
    }

    #[test]
    fn pending_operations_and_uncertain_visuals_keep_the_entire_wait() {
        let mut filter = Inactivity::default();
        for i in 0..=100 {
            let d = filter.select(i, i as f64 * 1000.0, true, i == 100);
            assert!(d.keep);
            assert!(d.tail.is_none());
        }
    }
}
