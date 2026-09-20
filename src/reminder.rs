//! 提醒引擎:扫出到点的提醒,并给出通知文案。
//!
//! 这里只做「算」,不做「弹」——发通知在 `platform.rs`,定时器在 `main.rs`。
//! 分开是为了能脱离界面直接跑单元测试。

use chrono::{DateTime, Local};

use crate::model::{Todo, MISSED_MAX_AGE_DAYS};

/// 一次扫描的结果
#[derive(Default)]
pub struct Fired {
    /// 该弹出来的任务(按到点时间排列)
    pub fresh: Vec<Todo>,
    /// 太老、只标记不弹的条数
    pub dropped: usize,
}

impl Fired {
    pub fn is_empty(&self) -> bool {
        self.fresh.is_empty() && self.dropped == 0
    }
}

/// 扫一遍所有任务,把「该提醒但还没提醒过」的挑出来并标记为已提醒。
///
/// 判据:`!done && !notified && remind_at <= now`。
/// 应用没运行时错过的提醒同样会被扫出来 —— 这就是「补提醒」,
/// 不需要单独的启动逻辑。
pub fn scan(todos: &mut [Todo], now: DateTime<Local>) -> Fired {
    let mut fired = Fired::default();
    for todo in todos.iter_mut() {
        if todo.done || todo.notified {
            continue;
        }
        let Some(at) = todo.remind_at() else {
            continue;
        };
        if at > now {
            continue;
        }

        todo.notified = true; // 标记了就必须落盘,否则重启会重弹
        if (now - at).num_days() >= MISSED_MAX_AGE_DAYS {
            fired.dropped += 1;
        } else {
            fired.fresh.push(todo.clone());
        }
    }
    fired.fresh.sort_by_key(|t| t.remind_at());
    fired
}

/// 通知的(标题, 正文)。1 条时带任务名,多条时合并成一条汇总,避免通知轰炸。
pub fn notify_content(fired: &Fired, now: DateTime<Local>) -> (String, String) {
    if fired.fresh.is_empty() {
        return (String::new(), String::new());
    }

    // 刚过点和「攒了一晚上 / 没开机」要区分开:后者不提一句的话,
    // 通知看着像是正在发生的事。
    let late = fired
        .fresh
        .iter()
        .filter_map(|t| t.remind_at())
        .map(|at| (now - at).num_minutes())
        .max()
        .unwrap_or(0)
        >= 60;

    let body = match fired.fresh.len() {
        1 => {
            let todo = &fired.fresh[0];
            let time = match todo.due.as_ref().and_then(|d| d.time_parsed()) {
                Some(time) => format!("{} ", time.format("%H:%M")),
                None => String::new(),
            };
            if late {
                format!("{time}{} · 已过期", todo.title)
            } else {
                format!("{time}{}", todo.title)
            }
        }
        n if late => format!("有 {n} 条提醒已过期,打开看看"),
        n => format!("有 {n} 条提醒"),
    };
    ("待办提醒".to_string(), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Due, DEFAULT_REMIND_BEFORE, NO_REMIND};
    use chrono::{NaiveDate, NaiveTime, TimeZone};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap();
        match Local.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => dt,
            chrono::LocalResult::Ambiguous(dt, _) => dt,
            chrono::LocalResult::None => panic!("测试时间落进了夏令时空洞"),
        }
    }

    fn task(id: i32, date: &str, time: Option<&str>) -> Todo {
        let mut t = Todo::new(id, format!("任务{id}"));
        t.due = Some(Due {
            date: date.into(),
            time: time.map(Into::into),
        });
        t
    }

    #[test]
    fn fires_only_after_remind_time() {
        let mut todos = vec![task(1, "2026-09-18", Some("09:30"))]; // 提前 15 分钟 → 09:15

        assert!(scan(&mut todos, at(2026, 9, 18, 9, 14)).is_empty());
        assert!(!todos[0].notified);

        let fired = scan(&mut todos, at(2026, 9, 18, 9, 15));
        assert_eq!(fired.fresh.len(), 1);
        assert!(todos[0].notified);
    }

    #[test]
    fn fires_at_most_once() {
        let mut todos = vec![task(1, "2026-09-18", Some("09:30"))];
        assert_eq!(scan(&mut todos, at(2026, 9, 18, 9, 16)).fresh.len(), 1);
        // 再扫多少次都不该重弹
        assert!(scan(&mut todos, at(2026, 9, 18, 9, 17)).is_empty());
        assert!(scan(&mut todos, at(2026, 9, 18, 10, 0)).is_empty());
    }

    #[test]
    fn missed_reminders_fire_on_next_launch() {
        // 应用一直没开,三天后才启动
        let mut todos = vec![task(1, "2026-09-15", Some("09:30"))];
        let fired = scan(&mut todos, at(2026, 9, 18, 12, 0));
        assert_eq!(fired.fresh.len(), 1);
        let (_, body) = notify_content(&fired, at(2026, 9, 18, 12, 0));
        assert!(body.contains("已过期"), "体:{body}");
    }

    #[test]
    fn ancient_reminders_are_marked_but_not_shown() {
        let mut todos = vec![task(1, "2026-09-01", Some("09:30"))]; // 17 天前
        let fired = scan(&mut todos, at(2026, 9, 18, 12, 0));
        assert!(fired.fresh.is_empty());
        assert_eq!(fired.dropped, 1);
        assert!(todos[0].notified, "标记了才不会再弹");
    }

    #[test]
    fn done_and_muted_tasks_never_fire() {
        let mut done = task(1, "2026-09-18", Some("09:30"));
        done.done = true;
        let mut muted = task(2, "2026-09-18", Some("09:30"));
        muted.remind_before = NO_REMIND;
        let mut todos = vec![done, muted];
        assert!(scan(&mut todos, at(2026, 9, 18, 12, 0)).is_empty());
    }

    #[test]
    fn several_at_once_merge_into_one_notification() {
        let mut todos = vec![
            task(1, "2026-09-18", Some("09:30")),
            task(2, "2026-09-18", Some("09:40")),
            task(3, "2026-09-18", Some("09:50")),
        ];
        let now = at(2026, 9, 18, 9, 41);
        let fired = scan(&mut todos, now);
        assert_eq!(fired.fresh.len(), 3, "同一批要一起弹");
        let (title, body) = notify_content(&fired, now);
        assert_eq!(title, "待办提醒");
        assert_eq!(body, "有 3 条提醒");
    }

    #[test]
    fn single_notification_carries_title_and_time() {
        let mut todos = vec![task(1, "2026-09-18", Some("09:30"))];
        let now = at(2026, 9, 18, 9, 15);
        let fired = scan(&mut todos, now);
        let (_, body) = notify_content(&fired, now);
        assert_eq!(body, "09:30 任务1");
    }

    #[test]
    fn changing_the_time_rearms_the_reminder() {
        let mut todos = vec![task(1, "2026-09-18", Some("09:30"))];
        assert_eq!(scan(&mut todos, at(2026, 9, 18, 9, 15)).fresh.len(), 1);
        // 用户把它改到下午
        todos[0].due = Some(Due {
            date: "2026-09-18".into(),
            time: Some("14:00".into()),
        });
        todos[0].reset_notified();
        assert_eq!(scan(&mut todos, at(2026, 9, 18, 12, 0)).fresh.len(), 0);
        assert_eq!(scan(&mut todos, at(2026, 9, 18, 13, 45)).fresh.len(), 1);
    }

    #[test]
    fn all_day_task_fires_at_nine() {
        let mut todos = vec![task(1, "2026-09-18", None)];
        assert!(scan(&mut todos, at(2026, 9, 18, 8, 59)).is_empty());
        assert_eq!(scan(&mut todos, at(2026, 9, 18, 9, 0)).fresh.len(), 1);
    }

    #[test]
    fn default_remind_before_is_15() {
        assert_eq!(task(1, "2026-09-18", Some("09:30")).remind_before, DEFAULT_REMIND_BEFORE);
    }
}
