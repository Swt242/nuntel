//! 任务数据模型与持久化。
//!
//! 时间一律用**本地时区**理解和存储(见 docs/calendar-reminders.md §6.1):
//! 存 "YYYY-MM-DD" / "HH:MM" 而不是 UTC 时间戳,这样时区设置或夏令时变化
//! 不会把「9:30 的会」变成 10:30。代价是不支持跨时区使用(一期明确不做)。

use std::path::{Path, PathBuf};

use chrono::{
    DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta,
    TimeZone, Weekday,
};
use serde::{Deserialize, Serialize};

/// 文件格式版本。v1 = 没有 time 字段的老版本。
pub const CURRENT_VERSION: i32 = 2;
/// 默认提前 15 分钟提醒
pub const DEFAULT_REMIND_BEFORE: i32 = 15;
/// 不提醒
pub const NO_REMIND: i32 = -1;
/// 全天任务在当天这个钟点提醒
const ALL_DAY_REMIND_HOUR: u32 = 9;
/// 超过这么多天的过期提醒不再补弹(用户早就不关心了)
pub const MISSED_MAX_AGE_DAYS: i64 = 7;

// ── 主题 ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    #[default]
    System,
    Light,
    Dark,
}

// ── 外观(实心 / 毛玻璃)───────────────────────────────────────────────
//
// 跟 ThemeName(亮暗)是**两个独立维度**,别合并:一共 2×2 种组合都得成立。
// 存的是不依赖界面的名字,推给 UI 时才换成 Slint 的 Appearance 枚举。

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppearanceName {
    Solid,
    /// 默认给玻璃:这是目前一直在用的观感,老数据文件没有这个字段时
    /// serde 会填 default,升上来不会突然变样。
    #[default]
    Glass,
    /// 工业风:全直角 + 发丝线 + 纸墨中性色 + 信号黄
    Industrial,
}

// ── 截止时间 ──────────────────────────────────────────────────────────

/// 截止时间。`time` 缺省表示「全天」。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Due {
    /// 本地日期,`YYYY-MM-DD`
    pub date: String,
    /// 本地时刻,`HH:MM`;没有 = 全天
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

impl Due {
    pub fn new(date: NaiveDate, time: Option<NaiveTime>) -> Self {
        Self {
            date: date.format("%Y-%m-%d").to_string(),
            time: time.map(|t| t.format("%H:%M").to_string()),
        }
    }

    pub fn date_parsed(&self) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(self.date.trim(), "%Y-%m-%d").ok()
    }

    pub fn time_parsed(&self) -> Option<NaiveTime> {
        self.time
            .as_deref()
            .and_then(|t| NaiveTime::parse_from_str(t.trim(), "%H:%M").ok())
    }

    pub fn is_all_day(&self) -> bool {
        self.time.is_none()
    }

    /// 截止时刻。全天任务按当天最后一分钟算,只用于判断「过没过期」。
    pub fn deadline(&self) -> Option<DateTime<Local>> {
        let date = self.date_parsed()?;
        let time = self
            .time_parsed()
            .unwrap_or_else(|| NaiveTime::from_hms_opt(23, 59, 0).unwrap());
        local(date.and_time(time))
    }

    /// 该在什么时候提醒(本地时刻)。
    ///
    /// - 定时任务:`截止时刻 - remind_before` 分钟
    /// - 全天任务:当天 09:00(忽略 remind_before)
    /// - `remind_before < 0`:不提醒
    pub fn remind_at(&self, remind_before: i32) -> Option<DateTime<Local>> {
        if remind_before < 0 {
            return None;
        }
        let date = self.date_parsed()?;
        match self.time_parsed() {
            Some(time) => {
                let at = local(date.and_time(time))?;
                Some(at - TimeDelta::minutes(remind_before as i64))
            }
            None => local(date.and_time(NaiveTime::from_hms_opt(ALL_DAY_REMIND_HOUR, 0, 0)?)),
        }
    }
}

/// 本地时区的朴素时间 → 绝对时刻。
/// 夏令时切换当天可能出现「不存在」或「重复」的时刻,取最早的那个,不让它变成 None。
fn local(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Some(dt),
        LocalResult::Ambiguous(dt, _) => Some(dt),
        LocalResult::None => None,
    }
}

// ── 任务 ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Todo {
    pub id: i32,
    pub title: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<Due>,
    /// 提前多少分钟提醒;`-1` = 不提醒
    #[serde(default = "default_remind_before", skip_serializing_if = "is_default_remind")]
    pub remind_before: i32,
    /// 这条的提醒是否已经弹过(防止重复弹、重启后重弹)
    #[serde(default, skip_serializing_if = "is_false")]
    pub notified: bool,
}

fn default_remind_before() -> i32 {
    DEFAULT_REMIND_BEFORE
}

fn is_default_remind(v: &i32) -> bool {
    *v == DEFAULT_REMIND_BEFORE
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl Todo {
    pub fn new(id: i32, title: String) -> Self {
        Self {
            id,
            title,
            done: false,
            due: None,
            remind_before: DEFAULT_REMIND_BEFORE,
            notified: false,
        }
    }

    /// 该任务的提醒时刻;`None` 表示不参与提醒
    pub fn remind_at(&self) -> Option<DateTime<Local>> {
        if self.done {
            return None;
        }
        self.due.as_ref()?.remind_at(self.remind_before)
    }

    /// 改过时间之后要重新计时,否则会被 notified 挡住再也不弹
    pub fn reset_notified(&mut self) {
        self.notified = false;
    }
}

// ── 整份数据 ──────────────────────────────────────────────────────────

#[derive(Default, Serialize, Deserialize)]
pub struct DataFile {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub theme: ThemeName,
    /// 界面外观。**故意不跟着 version 升版本号** —— 这是个带 serde default 的
    /// 新增可选字段,老文件照样能读、读出来就是默认的 Glass,不需要迁移。
    /// 升版本号反而会把用户已有的 todos.json.v1.bak 覆盖掉(见 load 里的 backup_v1)。
    #[serde(default)]
    pub appearance: AppearanceName,
    /// 要不要显示桌宠。默认显示 —— 老数据文件没有这个字段时 serde 填 true,
    /// 升级上来就能看到桌宠(不想要的话托盘里点一下就行)。
    #[serde(default = "default_true")]
    pub show_pet: bool,
    /// 桌宠显示边长(逻辑像素)。设置窗口里可调。
    #[serde(default = "default_pet_size")]
    pub pet_size: f32,
    /// 桌宠动画速度,**每个动画一份**(1.0 = 素材原始速度,越大播得越快)。
    ///
    /// 键是动画名(`idle-1` / `read` / `shop` …,来自 `assets/pet/manifest.json`),
    /// 没有条目的动画按 1.0 走 —— 所以换素材、加动画都不用动这里,
    /// 新动画默认就是原速。
    ///
    /// 早先是一个全局的 `pet_speed` 标量,改成 map 之后那个字段就废弃了
    /// (老 JSON 里还留着也无所谓:读的时候 serde 直接忽略,写盘时自然消失)。
    #[serde(default)]
    pub pet_speeds: std::collections::HashMap<String, f32>,
}

fn default_true() -> bool {
    true
}

fn default_pet_size() -> f32 {
    PET_SIZE_DEFAULT
}

// ── 桌宠外观参数(设置窗口可调,存盘)────────────────────────────────
//
// 默认值就是原来写死的那个 112px;范围卡在下面的上下限里,
// 读盘时还会 clamp 一次 —— 手改 JSON 改出个 10000,不该让宠物撑满屏幕。

pub const PET_SIZE_DEFAULT: f32 = 112.0;
pub const PET_SIZE_MIN: f32 = 64.0;
pub const PET_SIZE_MAX: f32 = 200.0;
pub const PET_SPEED_DEFAULT: f32 = 1.0;
pub const PET_SPEED_MIN: f32 = 0.25;
pub const PET_SPEED_MAX: f32 = 3.0;

/// 把从盘里读到的桌宠参数夹进合法范围
pub fn clamp_pet_size(v: f32) -> f32 {
    if v.is_finite() { v.clamp(PET_SIZE_MIN, PET_SIZE_MAX) } else { PET_SIZE_DEFAULT }
}

pub fn clamp_pet_speed(v: f32) -> f32 {
    if v.is_finite() { v.clamp(PET_SPEED_MIN, PET_SPEED_MAX) } else { PET_SPEED_DEFAULT }
}

// ── AI 对话的接口配置 ────────────────────────────────────────────────
//
// **故意单独一个文件**:密钥和待办数据放一起的话,用户想贴个 todos.json 让人看
// 问题、或者哪天把数据目录同步到什么地方,密钥就跟着跑了。分开存至少不会顺手带出去。
//
// 也**没有用系统 keyring**:那会多一个 Windows 凭据管理器的依赖和一堆失败分支
// (凭据服务被策略关掉、企业环境里不可用…),对一个本地小工具不划算。
// 明文放在 %APPDATA% 下,权限跟其它用户数据一样 —— 界面上写明了这一点。

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AiConfig {
    /// 接口根地址,例如 https://api.openai.com/v1(补 /chat/completions 由 ai::endpoint 做)
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
}

impl AiConfig {
    /// 三项齐了才能发请求(缺哪项由界面提示)
    pub fn is_ready(&self) -> bool {
        self.missing().is_none()
    }

    /// 缺哪一项 —— 用来拼「还差 XXX」的提示
    pub fn missing(&self) -> Option<&'static str> {
        if self.base_url.trim().is_empty() {
            Some("接口地址")
        } else if self.api_key.trim().is_empty() {
            Some("密钥")
        } else if self.model.trim().is_empty() {
            Some("模型名")
        } else {
            None
        }
    }
}

/// Markdown 草稿本的文件路径,和 todos.json 同一个目录。
///
/// **没做文件管理**:用户选的是「单草稿本」,一个文件、随手记,
/// 所以不引文件对话框依赖、也不做笔记列表。
pub fn notes_file() -> PathBuf {
    data_file().with_file_name("notes.md")
}

/// AI 配置文件路径,和 todos.json 同一个目录
pub fn ai_config_file() -> PathBuf {
    data_file().with_file_name("ai.json")
}

/// 读 AI 配置。文件不在或内容坏了就给空配置(界面上会提示还没配好)。
pub fn load_ai(path: &Path) -> AiConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return AiConfig::default();
    };
    match serde_json::from_str(&text) {
        Ok(cfg) => cfg,
        Err(err) => {
            crate::platform::log(&format!("{} 解析失败,当没配过处理: {err}", path.display()));
            AiConfig::default()
        }
    }
}

/// 存 AI 配置。和待办一样:先写临时文件再改名,写一半崩了不会毁掉旧配置。
pub fn save_ai(path: &Path, cfg: &AiConfig) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化失败: {e}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("写入 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("改名到 {} 失败: {e}", path.display()))
}

/// 数据文件路径:`%APPDATA%\rgui-todo\todos.json`(非 Windows 上退回 XDG/HOME)
pub fn data_file() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rgui-todo").join("todos.json")
}

/// 读盘。文件不存在或内容坏了都当空清单处理,不让应用起不来。
///
/// 顺带做一次 v1 → v2 的备份:老文件在写回之前先留一份 `todos.json.v1.bak`。
pub fn load(path: &Path) -> DataFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return DataFile::default(); // 第一次运行还没这个文件
    };
    let data: DataFile = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(err) => {
            crate::platform::log(&format!("{} 内容无法解析,先当空清单处理: {err}", path.display()));
            return DataFile::default();
        }
    };

    if data.version < CURRENT_VERSION {
        backup_v1(path, &text);
    }
    data
}

fn backup_v1(path: &Path, original: &str) {
    let backup = path.with_extension("json.v1.bak");
    if backup.exists() {
        return; // 只备份一次
    }
    match std::fs::write(&backup, original) {
        Ok(()) => crate::platform::log(&format!("已把旧格式数据备份到 {}", backup.display())),
        Err(err) => crate::platform::log(&format!("备份到 {} 失败: {err}", backup.display())),
    }
}

/// 落盘。先写临时文件再改名,中途出错也不会留下半个 JSON 把数据毁掉。
pub fn save(path: &Path, data: &DataFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(data).map_err(|e| format!("序列化失败: {e}"))?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("写入 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("改名到 {} 失败: {e}", path.display()))
}

// ── 给界面用的小工具 ──────────────────────────────────────────────────

/// 任务行右边那个时间徽标的文案
pub struct Badge {
    pub text: String,
    /// 过期未完成 → 界面上标红
    pub overdue: bool,
}

/// 根据当前时间算出徽标文案;返回 `None` 表示不显示(无日期或已完成)。
pub fn badge(todo: &Todo, now: DateTime<Local>) -> Option<Badge> {
    if todo.done {
        return None;
    }
    let due = todo.due.as_ref()?;
    let date = due.date_parsed()?;
    let time = due.time.clone().unwrap_or_default();
    let with_time = |prefix: &str| {
        if time.is_empty() {
            prefix.to_string()
        } else {
            format!("{prefix} {time}")
        }
    };

    let today = now.date_naive();
    let days = (date - today).num_days();

    if days < 0 {
        let text = if days == -1 {
            "已过期 · 昨天".to_string()
        } else {
            format!("已过期 · {}月{}日", date.month(), date.day())
        };
        return Some(Badge { text, overdue: true });
    }
    if days == 0 {
        return Some(Badge { text: with_time("今天"), overdue: false });
    }
    if days == 1 {
        return Some(Badge { text: with_time("明天"), overdue: false });
    }
    if days < 7 {
        return Some(Badge {
            text: with_time(weekday_name(date.weekday())),
            overdue: false,
        });
    }
    Some(Badge {
        text: format!("{}月{}日", date.month(), date.day()),
        overdue: false,
    })
}

pub fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        local(NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()).unwrap()
    }

    #[test]
    fn timed_reminder_subtracts_lead_minutes() {
        let due = Due::new(NaiveDate::from_ymd_opt(2026, 9, 19).unwrap(), NaiveTime::from_hms_opt(9, 30, 0));
        assert_eq!(due.remind_at(15).unwrap(), at(2026, 9, 19, 9, 15));
        assert_eq!(due.remind_at(0).unwrap(), at(2026, 9, 19, 9, 30));
        assert_eq!(due.remind_at(NO_REMIND), None);
    }

    #[test]
    fn all_day_reminder_is_nine_am() {
        let due = Due {
            date: "2026-09-19".into(),
            time: None,
        };
        assert!(due.is_all_day());
        assert_eq!(due.remind_at(15).unwrap(), at(2026, 9, 19, 9, 0));
    }

    #[test]
    fn v1_json_still_loads() {
        // 老版本只有 id/title/done
        let old = r#"{"todos":[{"id":1,"title":"老的","done":true}],"theme":"dark"}"#;
        let data: DataFile = serde_json::from_str(old).unwrap();
        assert_eq!(data.version, 0);
        assert_eq!(data.todos.len(), 1);
        assert!(data.todos[0].done);
        assert!(data.todos[0].due.is_none());
        assert_eq!(data.todos[0].remind_before, DEFAULT_REMIND_BEFORE);
        assert!(!data.todos[0].notified);
    }

    #[test]
    fn badge_shows_overdue_and_relative_days() {
        let now = at(2026, 9, 18, 12, 0);
        let mk = |date: &str, time: Option<&str>| Todo {
            id: 1,
            title: "x".into(),
            done: false,
            due: Some(Due {
                date: date.into(),
                time: time.map(Into::into),
            }),
            remind_before: DEFAULT_REMIND_BEFORE,
            notified: false,
        };
        assert_eq!(badge(&mk("2026-09-18", Some("09:30")), now).unwrap().text, "今天 09:30");
        assert_eq!(badge(&mk("2026-09-19", None), now).unwrap().text, "明天");
        assert_eq!(badge(&mk("2026-09-21", Some("14:00")), now).unwrap().text, "周一 14:00");
        let late = badge(&mk("2026-09-16", None), now).unwrap();
        assert!(late.overdue);
        assert_eq!(late.text, "已过期 · 9月16日");
        assert_eq!(badge(&mk("2026-10-02", None), now).unwrap().text, "10月2日");

        let mut done = mk("2026-09-18", None);
        done.done = true;
        assert!(badge(&done, now).is_none());
    }

    #[test]
    fn completed_or_unreminded_tasks_do_not_fire() {
        let mut todo = Todo::new(1, "x".into());
        todo.due = Some(Due::new(NaiveDate::from_ymd_opt(2026, 9, 19).unwrap(), None));
        assert!(todo.remind_at().is_some());
        todo.remind_before = NO_REMIND;
        assert!(todo.remind_at().is_none());
        todo.remind_before = DEFAULT_REMIND_BEFORE;
        todo.done = true;
        assert!(todo.remind_at().is_none());
    }
}
