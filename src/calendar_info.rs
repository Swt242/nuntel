//! 日历格子上的附加信息:农历、节日、放假 / 调休。
//!
//! 数据都是离线的,不联网:
//! - 农历用 `lunar-lite` 现算;
//! - 法定节假日与调休用 `chinese_holiday`,它内置了 2000–2026 的安排。
//!
//! ⚠️ `chinese_holiday` 的入参超出 2004-01-01..2026-12-25 会 **assert 直接 panic**,
//! 所以这里必须先判范围 —— 用户把日历翻到 2027 年就会踩到。范围外退化成
//! 「周六周日休息」的朴素判断。

use chinese_holiday::{DayKind, chinese_holiday};
use chrono::{Datelike, NaiveDate, Weekday};
use lunar_lite::{SolarDate, solar_to_lunar};

/// `chinese_holiday` 支持的日期范围(含两端)
fn holiday_range_ok(date: NaiveDate) -> bool {
    let (y, m, d) = (date.year(), date.month(), date.day());
    (y, m, d) >= (2004, 1, 1) && (y, m, d) <= (2026, 12, 25)
}

/// 一天在日历上的标注
#[derive(Default, Clone, Debug, PartialEq)]
pub struct DayInfo {
    /// 农历文案:「初二」「八月」「闰六月」;算不出来就是空
    pub lunar: String,
    /// 要显示的节日名(「春节」「中秋」…),调休上班的日子是「班」;空 = 没有
    pub festival: String,
    /// 放假(法定假日或普通周末)
    pub rest: bool,
    /// 调休上班日
    pub work: bool,
}

impl DayInfo {
    /// 格子里该显示哪段小字:有节日显示节日,否则显示农历
    pub fn label(&self) -> &str {
        if !self.festival.is_empty() {
            &self.festival
        } else {
            &self.lunar
        }
    }
}

pub fn day_info(date: NaiveDate) -> DayInfo {
    let mut info = DayInfo {
        lunar: lunar_text(date),
        ..Default::default()
    };

    if holiday_range_ok(date) {
        match chinese_holiday(&date) {
            // 普通周末:只是休息,不标字
            DayKind::NormalHoliday => info.rest = true,
            DayKind::NormalWorkday => {}

            DayKind::NewYearsDayHoliday => mark_rest(&mut info, "元旦"),
            DayKind::SpringFestivalHoliday => mark_rest(&mut info, "春节"),
            DayKind::ChingMingFestivalHoliday => mark_rest(&mut info, "清明"),
            DayKind::InternationalWorkersDayHoliday => mark_rest(&mut info, "劳动节"),
            DayKind::DragonBoatFestivalHoliday => mark_rest(&mut info, "端午"),
            DayKind::MidAutumnFestivalHoliday => mark_rest(&mut info, "中秋"),
            DayKind::NationalDayHoliday => mark_rest(&mut info, "国庆"),
            DayKind::OtherHoliday => info.rest = true,

            DayKind::NewYearsDayWorkday
            | DayKind::SpringFestivalWorkday
            | DayKind::ChingMingFestivalWorkday
            | DayKind::InternationalWorkersDayWorkday
            | DayKind::DragonBoatFestivalWorkday
            | DayKind::MidAutumnFestivalWorkday
            | DayKind::NationalDayWorkday
            | DayKind::OtherWorkday => {
                info.work = true;
                info.festival = "班".to_string();
            }
        }
    } else {
        // 库不覆盖的年份:至少把周末标出来
        info.rest = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
    }

    // 法定假日表里没有的,补农历节日;再没有就补公历节日
    if info.festival.is_empty() {
        if let Some(name) = lunar_festival(date) {
            info.festival = name.to_string();
        }
    }
    if info.festival.is_empty() {
        if let Some(name) = solar_festival(date) {
            info.festival = name.to_string();
        }
    }
    info
}

fn mark_rest(info: &mut DayInfo, name: &str) {
    info.rest = true;
    info.festival = name.to_string();
}

// ── 农历 ──────────────────────────────────────────────────────────────

fn to_solar_date(date: NaiveDate) -> SolarDate {
    SolarDate {
        year: date.year(),
        month: date.month() as u8,
        day: date.day() as u8,
    }
}

/// 中文日历的惯例:初一显示月份名,其余显示「初二」「十五」「廿三」这种
fn lunar_text(date: NaiveDate) -> String {
    let Ok(lunar) = solar_to_lunar(to_solar_date(date)) else {
        return String::new();
    };
    if lunar.day == 1 {
        format!("{}{}", if lunar.is_leap_month { "闰" } else { "" }, month_name(lunar.month))
    } else {
        day_name(lunar.day).to_string()
    }
}

fn month_name(month: u8) -> &'static str {
    match month {
        1 => "正月",
        2 => "二月",
        3 => "三月",
        4 => "四月",
        5 => "五月",
        6 => "六月",
        7 => "七月",
        8 => "八月",
        9 => "九月",
        10 => "十月",
        11 => "冬月",
        _ => "腊月",
    }
}

fn day_name(day: u8) -> &'static str {
    const EARLY: [&str; 10] = [
        "初一", "初二", "初三", "初四", "初五", "初六", "初七", "初八", "初九", "初十",
    ];
    const TEENS: [&str; 9] = [
        "十一", "十二", "十三", "十四", "十五", "十六", "十七", "十八", "十九",
    ];
    const LATE: [&str; 9] = [
        "廿一", "廿二", "廿三", "廿四", "廿五", "廿六", "廿七", "廿八", "廿九",
    ];
    match day {
        1..=10 => EARLY[(day - 1) as usize],
        11..=19 => TEENS[(day - 11) as usize],
        20 => "二十",
        21..=29 => LATE[(day - 21) as usize],
        _ => "三十",
    }
}

/// 农历节日(按农历月日)
fn lunar_festival(date: NaiveDate) -> Option<&'static str> {
    let lunar = solar_to_lunar(to_solar_date(date)).ok()?;
    if lunar.is_leap_month {
        return None; // 闰月不算节日
    }
    // 除夕:第二天是正月初一
    if let Some(next) = date.succ_opt() {
        if let Ok(n) = solar_to_lunar(to_solar_date(next)) {
            if n.month == 1 && n.day == 1 && !n.is_leap_month {
                return Some("除夕");
            }
        }
    }
    match (lunar.month, lunar.day) {
        (1, 1) => Some("春节"),
        (1, 15) => Some("元宵"),
        (2, 2) => Some("龙抬头"),
        (5, 5) => Some("端午"),
        (7, 7) => Some("七夕"),
        (7, 15) => Some("中元"),
        (8, 15) => Some("中秋"),
        (9, 9) => Some("重阳"),
        (12, 8) => Some("腊八"),
        _ => None,
    }
}

/// 公历节日(法定假期表里没有的才用它兜底)
fn solar_festival(date: NaiveDate) -> Option<&'static str> {
    match (date.month(), date.day()) {
        (1, 1) => Some("元旦"),
        (2, 14) => Some("情人节"),
        (3, 8) => Some("妇女节"),
        (5, 1) => Some("劳动节"),
        (5, 4) => Some("青年节"),
        (6, 1) => Some("儿童节"),
        (7, 1) => Some("建党节"),
        (8, 1) => Some("建军节"),
        (9, 10) => Some("教师节"),
        (10, 1) => Some("国庆节"),
        (12, 25) => Some("圣诞节"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn lunar_day_and_month_names() {
        // 2026 春节是 2 月 17 日(丙午年正月初一)
        let spring = day_info(d(2026, 2, 17));
        assert_eq!(spring.lunar, "正月", "初一要显示月份名");
        assert_eq!(spring.festival, "春节");
        assert!(spring.rest);
        // 正月十六
        assert_eq!(day_info(d(2026, 3, 4)).lunar, "十六");
        // 二十
        assert_eq!(day_info(d(2025, 8, 13)).lunar, "二十");
    }

    #[test]
    fn legal_holidays_and_makeup_workdays() {
        // 2026-05-01 劳动节
        let labour = day_info(d(2026, 5, 1));
        assert_eq!(labour.festival, "劳动节");
        assert!(labour.rest && !labour.work);

        // 2026-05-09 是劳动节调休上班(周六要上班)
        let makeup = day_info(d(2026, 5, 9));
        assert!(makeup.work, "这天应该是调休上班");
        assert_eq!(makeup.festival, "班");
        assert!(!makeup.rest);
    }

    #[test]
    fn ordinary_weekend_has_no_label() {
        // 2026-09-19 是周六,没有节日
        let sat = day_info(d(2026, 9, 19));
        assert!(sat.rest);
        assert!(sat.festival.is_empty(), "普通周末不该标节日:{}", sat.festival);
        assert!(!sat.lunar.is_empty(), "农历还是要有的");
    }

    #[test]
    fn mid_autumn_2026() {
        // 2026 中秋是 9 月 25 日
        let mid = day_info(d(2026, 9, 25));
        assert_eq!(mid.festival, "中秋");
        assert!(mid.rest);
    }

    #[test]
    fn does_not_panic_outside_supported_years() {
        // 库只支持到 2026-12-25,超出去必须优雅退化而不是崩
        let future = day_info(d(2027, 3, 1));
        assert!(!future.rest, "2027-03-01 是周一,应判为工作日");
        assert!(!future.lunar.is_empty(), "农历照样要算得出来");
        let weekend = day_info(d(2030, 6, 2));
        assert!(weekend.rest, "2030-06-02 是周日");
        // 下界之前
        let _ = day_info(d(1999, 1, 1));
    }
}
