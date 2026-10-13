// SPDX-License-Identifier: GPL-3.0-only

use chrono::{Datelike, Days, Months, NaiveDate};

#[derive(Clone, Copy, Debug)]
pub struct Calendar {
    pub month: NaiveDate,
    pub selected: NaiveDate,
}

impl Calendar {
    pub fn new(today: NaiveDate) -> Self {
        Self {
            month: today.with_day(1).unwrap_or(today),
            selected: today,
        }
    }

    pub fn navigate(&mut self, forward: bool) {
        let next = if forward {
            self.month.checked_add_months(Months::new(1))
        } else {
            self.month.checked_sub_months(Months::new(1))
        };
        if let Some(next) = next {
            self.month = next;
        }
    }

    pub fn select(&mut self, date: NaiveDate) {
        self.selected = date;
        self.month = date.with_day(1).unwrap_or(date);
    }

    pub fn days(&self, monday: bool) -> Vec<NaiveDate> {
        let offset = if monday {
            self.month.weekday().num_days_from_monday()
        } else {
            self.month.weekday().num_days_from_sunday()
        };
        let Some(start) = self.month.checked_sub_days(Days::new(u64::from(offset))) else {
            return Vec::new();
        };
        (0..42)
            .filter_map(|day| start.checked_add_days(Days::new(day)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn leap_february_has_real_days_and_monday_alignment() {
        let calendar = Calendar::new(date(2024, 2, 29));
        let days = calendar.days(true);
        assert_eq!(days[0], date(2024, 1, 29));
        assert_eq!(days.iter().filter(|day| day.month() == 2).count(), 29);
        assert_eq!(days.len(), 42);
    }

    #[test]
    fn sunday_first_and_iso_year_boundary_are_correct() {
        let days = Calendar::new(date(2021, 1, 1)).days(false);
        assert_eq!(days[0], date(2020, 12, 27));
        assert_eq!(days[1].iso_week().week(), 53);
        assert_eq!(days[8].iso_week().week(), 1);
    }

    #[test]
    fn navigation_crosses_year_without_changing_selection() {
        let mut calendar = Calendar::new(date(2025, 12, 31));
        calendar.navigate(true);
        assert_eq!(calendar.month, date(2026, 1, 1));
        assert_eq!(calendar.selected, date(2025, 12, 31));
        calendar.navigate(false);
        assert_eq!(calendar.month, date(2025, 12, 1));
    }

    #[test]
    fn selecting_adjacent_month_updates_displayed_month() {
        let mut calendar = Calendar::new(date(2026, 1, 1));
        calendar.select(date(2025, 12, 29));
        assert_eq!(calendar.month, date(2025, 12, 1));
    }
}
