//! 待办清单 —— Rust 业务逻辑 + Slint 界面
//!
//! 分工:UI 只负责渲染和转发交互,任务数据、过滤条件、视图状态、时间编辑状态
//! 全部在这里持有,每次变更后重建给 UI 的模型并写盘。
//!
//! - `model.rs`    任务数据结构与读写盘
//! - `reminder.rs` 到点判定与通知文案
//! - `platform.rs` 单实例、托盘、系统通知、开机自启
//!
//! 任务用稳定 `id` 标识,UI 回调都传 id 而不是下标 —— 过滤/换视图后下标会变,
//! 用 id 就不会点错行。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod calendar_info;
mod model;
mod pet;
mod platform;
mod reminder;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, NaiveDate, NaiveTime, Timelike};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use model::{AppearanceName, DataFile, Due, ThemeName, Todo};
use platform::TrayAction;

slint::include_modules!();

/// 日历网格固定 6 周 × 7 天
const CALENDAR_ROWS: i64 = 6;
const CALENDAR_COLS: i64 = 7;
/// 一格里最多列几条任务,多的显示「还有 N 条」
const CELL_MAX_LINES: usize = 2;
/// 时间浮层里用哪一套日期/时刻选择器:
///
/// - `true`  = Slint 自带的 `DatePickerPopup` / `TimePickerPopup`(表盘式,成熟)
/// - `false` = 自己写的(月历 + 滚轮,配色跟三套主题完全一致)
///
/// 两套 UI 代码都留在 `ui/app.slint` 里,改这一个常量就能来回切。
///
/// 试过内置那套(A)之后的结论:它能用,但**不是滚轮**(时刻那个是表盘),
/// 而且吃 `FluentPalette` —— 在暗色工业风上会弹出一块纯白面板。
/// std-widgets 里也没有别的可选项(SpinBox / ComboBox 同病,都不是滚轮),
/// 所以回到自写这套。
const USE_NATIVE_PICKERS: bool = false;

// ── 滚轮的惯性滑动(阻尼)─────────────────────────────────────────────
//
// 模型用**指数衰减**,和 iOS UIScrollView 是一路子:
//
//     v(t) = v₀ · e^(−k·t)
//     x(t) = x₀ + (v₀/k)·(1 − e^(−k·t))
//
// 每帧用增量形式推进(和闭式解等价,但更适合逐帧):
//
//     v ← v · RETAIN^dt        速度按时间衰减
//     x ← x + v·dt             位置按速度积分
//
// 速度掉到阈值以下就停,然后吸附到最近一整格 —— 这就是"滑动力"。
//
// **为什么必须放在宿主里**:阻尼要按**时间**积分,而 Slint 语言读不到时钟,
// 既估不出松手时的速度,也做不了逐帧推进。所以位置本身也归宿主管。

/// 惯性推进的步长(≈60fps)
const FLING_TICK: Duration = Duration::from_millis(16);
/// 速度每秒**保留**的比例。越小停得越快。
/// 0.01 表示一秒后只剩 1%,滑行总距离 ≈ v₀/k,其中 k = −ln(0.01) ≈ 4.6/s。
const FLING_RETAIN_PER_SEC: f32 = 0.01;
/// 低于这个速度(格/秒)就认为滑完了,进入吸附收尾
const FLING_MIN_SPEED: f32 = 0.8;
/// 手指最后一次移动之后过了这么久才松手 → 认为它是"停住后抬起",不算甩动
const DRAG_STALE: Duration = Duration::from_millis(70);
/// 估速度时给上一次估计的权重。指针事件间隔不均匀,不平滑会抖。
const VELOCITY_SMOOTHING: f32 = 0.35;
/// **两次采样至少间隔这么久才算数。**
/// 同一帧里连着送来两个 move 的话,`dt` 会接近 0 而 Δ 是真实的 ——
/// `Δ/dt` 直接飞出天际,惯性会把轮子一把甩到底。宁可少采几次,也不能采这种。
const VELOCITY_MIN_DT: f32 = 0.010;
/// 速度上限(格/秒)。兜底用。
/// 35 格/秒 对应滑行约 7.6 格 —— 一个 24 格的轮子,一甩滑过去三分之一已经够猛了,
/// 再放开就成"甩飞"了。
const VELOCITY_MAX: f32 = 35.0;

/// 指数衰减系数 k(单位 1/秒)
fn fling_k() -> f32 {
    -FLING_RETAIN_PER_SEC.ln()
}

/// 按当前速度还会滑多远(格)。闭式解 v₀/k,用来给参数定标。
fn fling_distance(velocity: f32) -> f32 {
    velocity / fling_k()
}

/// 推进 dt 秒之后的速度
fn fling_step(velocity: f32, dt_sec: f32) -> f32 {
    velocity * FLING_RETAIN_PER_SEC.powf(dt_sec)
}

/// 采一次速度。
///
/// **间隔太短的采样直接丢弃**(返回上一次的速度):同一帧里连着来两个 move 时
/// `dt` 接近 0,而位移是真的,`Δ/dt` 会算出几千格/秒 —— 惯性一下就把轮子甩到底。
/// 丢弃之后 `last_pos` / `last_at` 不推进,下一次采样自然覆盖更长的窗口。
/// 再夹一个上限做兜底。
fn velocity_sample(prev_velocity: f32, prev_pos: f32, pos: f32, dt_sec: f32) -> f32 {
    if dt_sec < VELOCITY_MIN_DT {
        return prev_velocity;
    }
    let sample = (pos - prev_pos) / dt_sec;
    let blended = prev_velocity * VELOCITY_SMOOTHING + sample * (1.0 - VELOCITY_SMOOTHING);
    blended.clamp(-VELOCITY_MAX, VELOCITY_MAX)
}

/// 滚轮的格子数:时 0-23,分 0-59
fn wheel_count(kind: i32) -> i32 {
    if kind == 0 { 24 } else { 60 }
}

/// 一个滚轮正在进行的拖动 / 滑行
struct WheelDrag {
    /// 0 = 时,1 = 分
    kind: i32,
    /// 当前浮点位置(单位:格)
    pos: f32,
    /// 速度(格/秒)
    velocity: f32,
    /// 上一次采样,用来估算速度
    last_pos: f32,
    last_at: Instant,
    /// 手指是不是还按着
    dragging: bool,
}

// ── 桌宠 ────────────────────────────────────────────────────────────────

/// 桌宠窗口宽度。比宠物宽,给提醒气泡和输入条留位置。
/// (宠物本身的边长是可调的,见 `model::PET_SIZE_DEFAULT` 那几个常量)
const PET_WIDTH: f32 = 240.0;
/// 动画心跳。比帧间隔细得多,由 `pet::advance` 自己累积 —— 见那个函数的说明。
const PET_TICK: Duration = Duration::from_millis(40);
/// 宠物脚底离屏幕右下角的边距(默认停靠位置)
const PET_MARGIN: i32 = 24;

/// 待机动画的驻留时间:每个待机动画至少播这么久才换下一个。
///
/// 一个 idle 循环大概 1~2 秒,不加这个的话每秒都在换人,像在抽搐。
/// 再叠一点随机抖动(见 `PET_IDLE_JITTER`),免得像节拍器。
const PET_IDLE_DWELL: Duration = Duration::from_secs(8);
const PET_IDLE_JITTER_MS: u64 = 7000;

/// 动画名的中文标签(设置窗口里显示)。**这是唯一需要人工维护的一张表** ——
/// 名字本身来自 `assets/pet/manifest.json`,这里只是给人看的说法。
/// 没配到的名字直接显示原名,所以加了新素材也不会漏。
const PET_ANIM_LABELS: &[(&str, &str)] = &[
    ("idle-1", "待机·挥手"),
    ("idle-2", "待机·抱枕"),
    ("idle-3", "待机·趴着"),
    ("idle-4", "待机·星星"),
    ("idle-5", "待机·害羞"),
    ("idle-6", "待机·雨衣"),
    ("idle-7", "待机·纸飞机"),
    ("idle-8", "待机·大衣"),
    ("read", "看书"),
    ("shop", "购物"),
];

/// 什么时候播哪个动画。**全部策略就是这一张表**,想换个风格改这里就行。
///
/// 除此之外的时间都在待机轮播(见 State::rotate_pet_idle):
/// 10 个动画里只有 2 个是交互专用的,剩下 8 个待机动画靠轮播才都用得上。
fn pet_anim_label(name: &str) -> String {
    PET_ANIM_LABELS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| name.to_string())
}

/// 一个够用的伪随机数(xorshift64)。
///
/// 只用来挑「下一个待机动画」,不值得为它引一个 rand 依赖。
/// 种子给 0 会退化成恒等于 0,所以播种时要避开 0。
fn next_rand(seed: &mut u64) -> u64 {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *seed = x;
    x
}

/// 提醒扫描间隔
const REMINDER_TICK: Duration = Duration::from_secs(20);
/// 托盘轮询间隔(tray-icon 用 channel 回调,得自己轮)
const TRAY_TICK: Duration = Duration::from_millis(300);

/// RemindOptions 下拉索引 → 提前的分钟数(跟 ui/widgets.slint 里的表一致)
fn remind_minutes(index: i32) -> i32 {
    match index {
        0 => model::NO_REMIND,
        1 => 0,
        2 => 5,
        3 => 15,
        4 => 30,
        5 => 60,
        _ => 1440,
    }
}

/// 存的是不依赖界面的 ThemeName,推给 UI 时换成 Slint 的枚举
fn theme_to_ui(name: ThemeName) -> ThemePreference {
    match name {
        ThemeName::System => ThemePreference::System,
        ThemeName::Light => ThemePreference::Light,
        ThemeName::Dark => ThemePreference::Dark,
    }
}

fn theme_from_ui(pref: ThemePreference) -> ThemeName {
    match pref {
        ThemePreference::Light => ThemeName::Light,
        ThemePreference::Dark => ThemeName::Dark,
        _ => ThemeName::System,
    }
}

/// 外观:存的名字 → Slint 枚举
fn appearance_to_ui(name: AppearanceName) -> Appearance {
    match name {
        AppearanceName::Solid => Appearance::Solid,
        AppearanceName::Glass => Appearance::Glass,
        AppearanceName::Industrial => Appearance::Industrial,
    }
}

/// 外观:Slint 枚举 → 存的名字(未知值一律当玻璃,免得将来加枚举项时炸)
fn appearance_from_ui(value: Appearance) -> AppearanceName {
    match value {
        Appearance::Solid => AppearanceName::Solid,
        Appearance::Industrial => AppearanceName::Industrial,
        _ => AppearanceName::Glass,
    }
}

/// "09:30" → (9, 30);解析不了给 None
fn parse_hhmm(text: &str) -> Option<(i32, i32)> {
    let t = NaiveTime::parse_from_str(text, "%H:%M").ok()?;
    Some((t.hour() as i32, t.minute() as i32))
}

/// 时刻选择器的候选:`{value, label}`。
///
/// `label` 必须在宿主补零成两位 —— Slint 没有字符串格式化,
/// 在那边拼出来是 "5" 而不是 "05",一排数字块就会参差不齐。
fn time_options(values: impl IntoIterator<Item = i32>) -> Vec<TimeOption> {
    values
        .into_iter()
        .map(|v| TimeOption {
            value: v,
            label: format!("{v:02}").as_str().into(),
        })
        .collect()
}

/// 选择器里的「时:分」→ `NaiveTime`。
///
/// 越界值兜底成 09:00。选择器本身不会给出越界值(格子是固定的 0-23 / 0-59,
/// 而且回调里还 clamp 过一次),这里只是不想在保存路径上再留一个 unwrap ——
/// 保存失败是没有 UI 兜底的,宁可存一个稳妥的时间。
fn picker_time(hour: i32, minute: i32) -> NaiveTime {
    NaiveTime::from_hms_opt(hour as u32, minute as u32, 0)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(9, 0, 0).unwrap())
}

/// 时间浮层的选择 → 存盘的 `Due`。
///
/// 从 UI 回调里拎出来单独放,是为了能直接单测:「全天」到底存不存时刻、
/// 选中的时分有没有正确落进去,这两件事不该只能靠点界面来验。
fn due_from_picker(date: NaiveDate, hour: i32, minute: i32, all_day: bool) -> Due {
    if all_day {
        // 全天任务不存时刻 —— 它在当天 09:00 提醒,这个 09:00 是提醒逻辑里的,
        // 不是用户选的,存进去反而会变成「用户设了 09:00」
        Due::new(date, None)
    } else {
        Due::new(date, Some(picker_time(hour, minute)))
    }
}

/// 提前的分钟数 → 下拉索引
fn remind_index(minutes: i32) -> i32 {
    if minutes < 0 {
        0
    } else if minutes == 0 {
        1
    } else if minutes <= 5 {
        2
    } else if minutes <= 15 {
        3
    } else if minutes <= 30 {
        4
    } else if minutes <= 60 {
        5
    } else {
        6
    }
}

// ── 应用状态 ──────────────────────────────────────────────────────────

struct State {
    ui: slint::Weak<AppWindow>,
    /// 唯一数据源(未过滤的全部任务)
    todos: RefCell<Vec<Todo>>,
    /// 清单视图的模型(过滤后)
    list_model: Rc<VecModel<TodoItem>>,
    /// 日历 42 格
    days_model: Rc<VecModel<DayCell>>,
    /// 日历里选中那天的任务
    day_model: Rc<VecModel<TodoItem>>,
    /// 周视图:7 天,每天带自己的任务列表
    week_model: Rc<VecModel<WeekDay>>,
    filter: Cell<Filter>,
    view: Cell<View>,
    editing: Cell<i32>,
    /// 时间浮层正在编辑的任务;-1 = 关着
    editor: Cell<i32>,
    /// 时间浮层里选中的日期
    editor_date: Cell<NaiveDate>,
    /// 时间浮层的月历正在显示哪个月(存该月 1 号)
    editor_month: Cell<NaiveDate>,
    /// 时间浮层里选中的时刻。**和日期一样由宿主持有**,不再是用户敲的字符串 ——
    /// 所以界面上那两个 TextInput 连同「解析失败」这条路径一起没了。
    editor_hour: Cell<i32>,
    editor_minute: Cell<i32>,
    /// 滚轮当前的拖动 / 滑行状态(同一时刻只有一个轮在动)
    wheel: RefCell<Option<WheelDrag>>,
    /// 惯性滑动的推进定时器。**只在上一次滑行还没停时跑**,停了就 stop()。
    fling_timer: RefCell<Option<Timer>>,
    /// 自引用,给定时器回调捕获用(State 建好之后再填)
    me: RefCell<std::rc::Weak<State>>,
    /// 时间浮层月历的 42 格
    editor_days_model: Rc<VecModel<PickerDay>>,
    /// 时刻候选(时 0-23 / 分 5 分钟一档)
    editor_hours_model: Rc<VecModel<TimeOption>>,
    editor_minutes_model: Rc<VecModel<TimeOption>>,
    /// 日历选中的那天
    cursor: Cell<NaiveDate>,
    /// 日历当前显示哪个月(存该月 1 号)
    month: Cell<NaiveDate>,
    /// 已经弹过通知、但用户还没确认的任务
    alerting: RefCell<Vec<i32>>,
    next_id: Cell<i32>,
    path: PathBuf,
    tray: RefCell<Option<platform::Tray>>,
    /// 上次算出来的「窗口是不是矮」,变了才推给 UI
    compact: Cell<bool>,
    /// 界面外观(实心 / 毛玻璃)
    appearance: Cell<AppearanceName>,
    /// 设置窗口还差一次「把它从任务栏里拿掉」的补丁,见 open_settings 的说明
    settings_fixup_pending: Cell<bool>,
    /// 桌宠窗口。跟设置窗口一样必须一直持有,且同样有自己一份 AppData/Logic。
    pet: RefCell<Option<Rc<PetWindow>>>,
    /// 解码好的动画素材(进程内只解一次)
    pet_frames: pet::PetFrames,
    /// (动画下标, 帧下标)
    pet_anim: Cell<(usize, usize)>,
    /// 累积到现在还差多少毫秒翻下一帧
    pet_left_ms: Cell<f64>,
    pet_timer: RefCell<Option<Timer>>,
    /// 是不是展开了快速添加输入条
    pet_adding: Cell<bool>,
    /// 桌宠的任务栏样式还差一次补丁(同 settings_fixup_pending)
    pet_fixup_pending: Cell<bool>,
    /// 要不要显示桌宠(托盘里可勾)
    show_pet: Cell<bool>,
    /// 拖动桌宠时记下的「按下瞬间的 (指针全局位置, 窗口位置)」。
    /// 见 pet_drag_begin 的说明 —— 拖动必须用全局坐标算位移。
    pet_drag: Cell<Option<((i32, i32), (i32, i32))>>,
    /// 这次按下有没有真的拖动过。用来吞掉拖动结束时顺带发出的那个 `clicked`
    /// (否则每拖一次都会顺手把「快速添加」输入条开一下)。
    pet_just_dragged: Cell<bool>,
    /// 桌宠显示边长(px)。可从设置窗口调,存盘。
    pet_size: Cell<f32>,
    /// 桌宠动画速度倍率,**每个动画一份**(名字 → 倍率;没有的按 1.0)。
    pet_speeds: RefCell<std::collections::HashMap<String, f32>>,
    /// 当前是不是在「待机轮播」状态。交互动画(看书/购物…)只播一轮就回到待机。
    pet_idle: Cell<bool>,
    /// 待机时,**最早什么时候可以换下一个动画**(见 PET_IDLE_DWELL)
    pet_idle_until: Cell<Instant>,
    /// xorshift 的种子,用来挑下一个待机动画
    pet_rng: Cell<u64>,
    /// 桌宠窗口上一次算出来的高度。只有它变了才动窗口 ——
    /// 不然每次同步都 set_position,用户拖着的时候会被拽回去。
    pet_h: Cell<f32>,
    /// 「让窗口整窗口重画一次」用的定时器。**必须留着**:
    /// Slint 的 Timer 一旦被 drop 就会停掉(踩过这个坑,见 main 里那段注释),
    /// 写成局部变量就等于定时器永远不触发。
    repaint_timers: RefCell<Vec<Timer>>,

    /// 设置窗口。**必须一直持有**,drop 掉窗口就没了;
    /// 用户点关闭只是 hide,下次直接 show,不重建(重建会闪一下、还可能丢位置)。
    ///
    /// 套一层 `Rc` 是因为 Slint 生成的窗口类型**没有实现 `Clone`**
    /// (内部是 `VRc`,但生成的结构体没 derive),借出去之后想再拿一份就只能靠 Rc。
    /// Rc 解引用之后照样能直接调 `show()` / `global()`。
    settings: RefCell<Option<Rc<SettingsWindow>>>,
}

impl State {
    // ── 模型构建 ──────────────────────────────────────────────────────

    /// 任务 → UI 用的行数据
    fn to_item(&self, todo: &Todo, now: chrono::DateTime<Local>) -> TodoItem {
        let badge = model::badge(todo, now);
        TodoItem {
            id: todo.id,
            title: todo.title.as_str().into(),
            done: todo.done,
            badge: badge.as_ref().map(|b| b.text.as_str()).unwrap_or("").into(),
            overdue: badge.map(|b| b.overdue).unwrap_or(false),
            alerting: self.alerting.borrow().contains(&todo.id),
        }
    }

    /// 重建清单视图的模型 + 计数 + 落盘。所有任务改动最后都汇到这里。
    fn sync(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return; // 窗口已经关了
        };
        let now = Local::now();
        let filter = self.filter.get();

        let (rows, remaining, completed) = {
            let todos = self.todos.borrow();
            let rows: Vec<TodoItem> = todos
                .iter()
                .filter(|t| match filter {
                    Filter::Active => !t.done,
                    Filter::Completed => t.done,
                    Filter::All => true,
                })
                .map(|t| self.to_item(t, now))
                .collect();
            let completed = todos.iter().filter(|t| t.done).count() as i32;
            let remaining = todos.len() as i32 - completed;
            (rows, remaining, completed)
        };

        self.list_model.set_vec(rows);
        self.sync_calendar();
        self.sync_week();
        self.sync_reminder_banner();

        let app = ui.global::<AppData>();
        app.set_filter(filter);
        app.set_remaining(remaining);
        app.set_completed(completed);

        self.push_pet(); // 角标上的未完成数跟着变

        self.save();
    }

    /// 重建日历的 42 格 + 选中日的任务列表 + 各种文案
    fn sync_calendar(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let now = Local::now();
        let today = now.date_naive();
        let cursor = self.cursor.get();
        let month = self.month.get();
        let todos = self.todos.borrow();

        // 该月 1 号所在周(周一起)的第一天
        let first = NaiveDate::from_ymd_opt(month.year(), month.month(), 1).unwrap_or(today);
        let start = first - chrono::Duration::days(first.weekday().num_days_from_monday() as i64);

        let mut cells = Vec::with_capacity((CALENDAR_ROWS * CALENDAR_COLS) as usize);
        for i in 0..CALENDAR_ROWS * CALENDAR_COLS {
            let date = start + chrono::Duration::days(i);
            let mut day_todos: Vec<&Todo> = todos
                .iter()
                .filter(|t| {
                    t.due
                        .as_ref()
                        .and_then(|d| d.date_parsed())
                        .map(|d| d == date)
                        .unwrap_or(false)
                })
                .collect();
            // 全天在前,再按时刻;已完成的沉到后面
            day_todos.sort_by_key(|t| {
                let due = t.due.as_ref().unwrap();
                (t.done, due.is_all_day() == false, due.time.clone().unwrap_or_default())
            });

            let lines: Vec<&Todo> = day_todos.iter().take(CELL_MAX_LINES).copied().collect();
            let mut it = lines.iter();
            let l1 = it.next().map(|t| self.to_item(t, now));
            let l2 = it.next().map(|t| self.to_item(t, now));

            let info = calendar_info::day_info(date);
            cells.push(DayCell {
                date: date.format("%Y-%m-%d").to_string().into(),
                day: date.day() as i32,
                in_month: date.month() == month.month() && date.year() == month.year(),
                is_today: date == today,
                is_selected: date == cursor,
                overdue: day_todos.iter().any(|t| {
                    !t.done && t.due.as_ref().and_then(|d| d.deadline()).map(|d| d < now).unwrap_or(false)
                }),
                count: day_todos.len() as i32,
                sub: info.label().into(),
                is_festival: !info.festival.is_empty() && !info.work,
                is_work: info.work,
                rest: info.rest,
                line1: l1.as_ref().map(|i| i.title.clone()).unwrap_or_default(),
                line1_done: l1.as_ref().map(|i| i.done).unwrap_or(false),
                line2: l2.as_ref().map(|i| i.title.clone()).unwrap_or_default(),
                line2_done: l2.as_ref().map(|i| i.done).unwrap_or(false),
                more: (day_todos.len().saturating_sub(CELL_MAX_LINES)) as i32,
            });
        }
        self.days_model.set_vec(cells);

        // 选中那天的列表
        let mut selected: Vec<TodoItem> = todos
            .iter()
            .filter(|t| {
                t.due
                    .as_ref()
                    .and_then(|d| d.date_parsed())
                    .map(|d| d == cursor)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| self.to_item(t, now))
            .collect();
        selected.sort_by_key(|i| (i.done, i.badge.clone()));
        let count = selected.len() as i32;
        self.day_model.set_vec(selected);

        let app = ui.global::<AppData>();
        app.set_month_label(
            format!("{} 年 {} 月", month.year(), month.month()).into(),
        );
        let label = if cursor == today {
            format!("{} 月 {} 日 · 今天", cursor.month(), cursor.day())
        } else {
            format!(
                "{} 月 {} 日 · {}",
                cursor.month(),
                cursor.day(),
                model::weekday_name(cursor.weekday())
            )
        };
        app.set_selected_label(label.into());
        app.set_selected_day_count(count);
    }

    /// 重建周视图:光标所在那一周的 7 天,每天一个格子(日期 + 农历/节日 + 当天任务)
    fn sync_week(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let now = Local::now();
        let today = now.date_naive();
        let cursor = self.cursor.get();
        let todos = self.todos.borrow();

        let monday = cursor - chrono::Duration::days(cursor.weekday().num_days_from_monday() as i64);
        let mut days: Vec<WeekDay> = Vec::with_capacity(7);

        for offset in 0..7 {
            let date = monday + chrono::Duration::days(offset);
            let info = calendar_info::day_info(date);
            let head = format!(
                "{} 月 {} 日 {}",
                date.month(),
                date.day(),
                model::weekday_name(date.weekday())
            );

            let mut day_todos: Vec<&Todo> = todos
                .iter()
                .filter(|t| {
                    t.due
                        .as_ref()
                        .and_then(|d| d.date_parsed())
                        .map(|d| d == date)
                        .unwrap_or(false)
                })
                .collect();
            // 全天在前,再按时刻;已完成的沉到后面
            day_todos.sort_by_key(|t| {
                let due = t.due.as_ref().unwrap();
                (t.done, due.is_all_day() == false, due.time.clone().unwrap_or_default())
            });

            let tasks: Vec<TodoItem> = day_todos.iter().map(|t| self.to_item(t, now)).collect();
            days.push(WeekDay {
                date: date.format("%Y-%m-%d").to_string().into(),
                label: if date == today { format!("{head} · 今天").into() } else { head.into() },
                sub: info.label().into(),
                is_today: date == today,
                rest: info.rest,
                work: info.work,
                is_festival: !info.festival.is_empty() && !info.work,
                count: tasks.len() as i32,
                tasks: Rc::new(VecModel::from(tasks)).into(),
            });
        }
        self.week_model.set_vec(days);

        let sunday = monday + chrono::Duration::days(6);
        let label = if monday.month() == sunday.month() {
            format!("{} 年 {} 月 {} 日 – {} 日", monday.year(), monday.month(), monday.day(), sunday.day())
        } else {
            format!(
                "{} 月 {} 日 – {} 月 {} 日",
                monday.month(),
                monday.day(),
                sunday.month(),
                sunday.day()
            )
        };
        ui.global::<AppData>().set_week_label(label.into());
    }

    /// 周视图翻周:把光标挪 7 天,月份也跟着走
    fn shift_week(&self, delta: i32) {
        let cursor = self.cursor.get() + chrono::Duration::days(delta as i64 * 7);
        self.cursor.set(cursor);
        self.month.set(NaiveDate::from_ymd_opt(cursor.year(), cursor.month(), 1).unwrap_or(cursor));
        self.sync_calendar();
        self.sync_week();
    }

    /// 提醒横幅
    fn sync_reminder_banner(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let ids = self.alerting.borrow().clone();
        let title = ids
            .first()
            .and_then(|id| {
                self.todos
                    .borrow()
                    .iter()
                    .find(|t| t.id == *id)
                    .map(|t| t.title.clone())
            })
            .unwrap_or_default();
        let app = ui.global::<AppData>();
        app.set_pending_count(ids.len() as i32);
        app.set_pending_title(title.into());
    }

    fn save(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let data = DataFile {
            version: model::CURRENT_VERSION,
            todos: self.todos.borrow().clone(),
            theme: theme_from_ui(ui.global::<Theme>().get_preference()),
            appearance: self.appearance.get(),
            show_pet: self.show_pet.get(),
            pet_size: self.pet_size.get(),
            pet_speeds: self.pet_speeds.borrow().clone(),
        };
        if let Err(err) = model::save(&self.path, &data) {
            platform::log(&format!("保存待办失败: {err}"));
        }
    }

    fn find_mut(&self, f: impl Fn(&Todo) -> bool) -> Option<std::cell::RefMut<'_, Todo>> {
        std::cell::RefMut::filter_map(self.todos.borrow_mut(), |v| v.iter_mut().find(|t| f(t))).ok()
    }

    fn title_of(&self, id: i32) -> Option<String> {
        self.todos.borrow().iter().find(|t| t.id == id).map(|t| t.title.clone())
    }

    // ── 任务操作 ──────────────────────────────────────────────────────

    fn add(&self, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);

        let mut todo = Todo::new(id, title.to_owned());
        // 日历视图下新建的任务直接落在选中的那天(全天)
        if self.view.get() == View::Calendar {
            todo.due = Some(Due::new(self.cursor.get(), None));
        }
        self.todos.borrow_mut().push(todo);

        if let Some(ui) = self.ui.upgrade() {
            ui.global::<AppData>().set_draft(SharedString::default());
        }
        self.sync();
    }

    fn toggle(&self, id: i32) {
        if let Some(mut todo) = self.find_mut(|t| t.id == id) {
            todo.done = !todo.done;
        }
        // 勾完就不再提醒它了
        if self.todos.borrow().iter().find(|t| t.id == id).map(|t| t.done).unwrap_or(false) {
            self.alerting.borrow_mut().retain(|i| *i != id);
            // 完成一条时给个轻快的反馈(「待机·趴着」那套);取消勾选就不打扰了
            self.play_pet_cue("idle-3");
        }
        self.sync();
    }

    fn remove(&self, id: i32) {
        self.todos.borrow_mut().retain(|t| t.id != id);
        self.alerting.borrow_mut().retain(|i| *i != id);
        if self.editing.get() == id {
            self.set_editing(-1);
        }
        if self.editor.get() == id {
            self.close_time_editor();
        }
        self.sync();
    }

    fn toggle_all(&self) {
        let all_done = {
            let todos = self.todos.borrow();
            !todos.is_empty() && todos.iter().all(|t| t.done)
        };
        for t in self.todos.borrow_mut().iter_mut() {
            t.done = !all_done;
        }
        if !all_done {
            self.alerting.borrow_mut().clear(); // 全勾完了,提醒也算处理了
        }
        self.sync();
    }

    fn clear_completed(&self) {
        self.todos.borrow_mut().retain(|t| !t.done);
        self.sync();
    }

    fn set_filter(&self, filter: Filter) {
        self.filter.set(filter);
        self.sync();
    }

    fn begin_edit(&self, id: i32) {
        let Some(title) = self.title_of(id) else { return };
        self.editing.set(id);
        if let Some(ui) = self.ui.upgrade() {
            let app = ui.global::<AppData>();
            app.set_edit_text(title.into());
            app.set_editing_id(id);
        }
    }

    /// 空内容视为放弃修改(保留原标题),不做删除 —— 避免误删。
    fn commit_edit(&self, text: &str) {
        let id = self.editing.get();
        if id < 0 {
            return;
        }
        let text = text.trim();
        if !text.is_empty() {
            if let Some(mut todo) = self.find_mut(|t| t.id == id) {
                todo.title = text.to_owned();
            }
        }
        self.set_editing(-1);
        self.sync();
    }

    fn set_editing(&self, id: i32) {
        self.editing.set(id);
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<AppData>().set_editing_id(id);
        }
    }

    /// 按当前实际观感切换:亮 -> 暗 -> 亮(不回到「跟随系统」,行为更可预期)
    fn cycle_theme(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let next = if ui.global::<Theme>().get_dark() {
            ThemePreference::Light
        } else {
            ThemePreference::Dark
        };
        ui.global::<Theme>().set_preference(next);
        self.push_theme();
        self.save();
    }

    // ── 主题与外观 ────────────────────────────────────────────────────

    /// 把主题(亮暗 + 外观)推给**每一个**窗口。
    ///
    /// 为什么要有这个函数:Slint 的全局单例**不跨窗口共享** ——
    /// 主窗口和设置窗口各持一份 Theme 实例。只改主窗口那份,设置窗口纹丝不动。
    /// 所以凡是动主题的地方(切亮暗、切外观、开设置窗口)都必须走这里,
    /// 别再出现直接 `ui.global::<Theme>().set_xxx(...)` 的写法。
    fn push_theme(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let preference = ui.global::<Theme>().get_preference();
        let appearance = appearance_to_ui(self.appearance.get());

        ui.global::<Theme>().set_appearance(appearance);

        // 把句柄 clone 出来再调,不占着 borrow —— 设置属性会跑绑定,
        // 万一哪条绑定绕回来又要借 settings,抱着 RefCell 的借用会直接 panic。
        let settings = self.settings.borrow().as_ref().cloned();
        if let Some(settings) = settings {
            let theme = settings.global::<Theme>();
            theme.set_preference(preference);
            theme.set_appearance(appearance);
        }
    }

    /// 切外观。改状态 → 推给两个窗口 → 跟着调窗口圆角 → 落盘。
    fn set_appearance(&self, value: Appearance) {
        let name = appearance_from_ui(value);
        if name == self.appearance.get() {
            return; // 点的就是当前这个,别白写一次盘
        }
        self.appearance.set(name);
        self.push_theme();
        platform::set_settings_rounding(name == AppearanceName::Glass);
        self.save();
    }

    // ── 设置窗口 ──────────────────────────────────────────────────────

    /// 打开设置窗口。窗口在启动时就建好了(见 main),这里只负责显示。
    fn open_settings(&self) {
        let settings = self.settings.borrow().as_ref().cloned();
        let Some(settings) = settings else { return };
        // 两个滑杆要显示当前值 —— 设置窗口有自己那份 AppData,不推就一直是默认值
        self.push_pet_settings();

        // 走 reveal 而不是 show:见 reveal 的说明,直接 show 会只画一部分
        if let Err(err) = self.reveal(&*settings) {
            platform::log(&format!("显示设置窗口失败: {err}"));
            return;
        }
        platform::set_settings_rounding(self.appearance.get() == AppearanceName::Glass);
        platform::bring_to_front(platform::SETTINGS_TITLE);

        // 「从任务栏拿掉」这件事**不能在这儿同步做**:winit 是异步把窗口属性
        // (装饰、扩展样式这些)写回系统的,show() 返回时它可能还没写完 ——
        // 这时设的 WS_EX_TOOLWINDOW 会被它随后的写入整个覆盖掉(实测就是这么失效的)。
        // 所以先记个标记,等下一轮轮询(约 300ms,winit 早写完了)再补。
        platform::hide_from_taskbar(platform::SETTINGS_TITLE); // 先试一次
        self.settings_fixup_pending.set(true);
    }

    fn close_settings(&self) {
        let settings = self.settings.borrow().as_ref().cloned();
        if let Some(settings) = settings {
            let _ = settings.hide();
        }
    }

    // ── 视图与日历 ────────────────────────────────────────────────────

    fn set_view(&self, view: View) {
        self.view.set(view);
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<AppData>().set_view(view);
        }
        self.sync_calendar();
    }

    fn shift_month(&self, delta: i32) {
        let month = self.month.get();
        let (y, m) = (month.year(), month.month() as i32 + delta);
        // 让 chrono 帮忙处理跨年
        let base = NaiveDate::from_ymd_opt(y, 1, 1).unwrap().with_month(1).unwrap();
        let shifted = base
            .checked_add_months(chrono::Months::new((m - 1).max(0) as u32))
            .unwrap_or(base);
        self.month.set(shifted);
        self.sync_calendar();
    }

    fn goto_today(&self) {
        let today = Local::now().date_naive();
        self.cursor.set(today);
        self.month.set(NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap());
        self.sync_calendar();
        self.sync_week();
    }

    fn select_day(&self, date: &str) {
        let Some(date) = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok() else {
            return;
        };
        self.cursor.set(date);
        self.sync_calendar();
        self.sync_week();
    }

    // ── 时间设置浮层 ──────────────────────────────────────────────────

    fn open_time_editor(&self, id: i32) {
        let Some((title, due, remind_before)) = self
            .todos
            .borrow()
            .iter()
            .find(|t| t.id == id)
            .map(|t| (t.title.clone(), t.due.clone(), t.remind_before))
        else {
            return;
        };
        self.editor.set(id);

        let today = Local::now().date_naive();
        let (date, hour, minute, all_day) = match &due {
            Some(d) => {
                let date = d.date_parsed().unwrap_or(today);
                // 没存时刻就是全天。默认停在 09:00 —— 用户一旦取消「全天」,
                // 选择器里已经指着 09:00 了,不用自己再点一遍。
                let (h, m) = d.time.as_deref().and_then(parse_hhmm).unwrap_or((9, 0));
                (date, h, m, d.is_all_day())
            }
            None => (today, 9, 0, true),
        };
        self.editor_date.set(date);
        self.editor_month
            .set(NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date));
        self.editor_hour.set(hour);
        self.editor_minute.set(minute);

        // ⚠️ 顺序要紧:**先把所有状态推完,最后再打开浮层**(`set_editor_id`)。
        //
        // 因为浮层是 `if editor-id >= 0` 创建出来的,它一出现,里面两个滚轮就会跑
        // `init => { pos = selected }` 去定位。要是这会儿 `editor-hour` 还没推,
        // 滚轮就停在上一轮的旧值上,而且之后不会再跟着改(滚轮的 pos 刻意不跟 selected
        // 持续同步,见 WheelColumn 的说明)—— 表现是"打开浮层,时间停在上次那条任务的值"。
        if let Some(ui) = self.ui.upgrade() {
            let app = ui.global::<AppData>();
            app.set_editor_title(title.into());
            app.set_editor_all_day(all_day);
            app.set_editor_remind(remind_index(remind_before));
            app.set_editor_had_due(due.is_some());
        }
        // 时刻和日期都得推:两个选择器的选中态都靠宿主推的属性点亮
        self.push_editor_time();
        self.build_editor_days();
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<AppData>().set_editor_id(id);
        }
    }

    fn close_time_editor(&self) {
        self.editor.set(-1);
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<AppData>().set_editor_id(-1);
        }
    }

    /// 重建时间浮层月历的 42 格。
    ///
    /// 和主日历的 `sync_calendar` 是两份数据(浮层可以自己翻到别的月份去),
    /// 但复用同一个 `calendar_info::day_info` —— 于是**在选择器里也能看出
    /// 哪几天是周末/法定假日**,挑日期时不会把"本来就不上班的那天"当成工作日。
    fn build_editor_days(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let today = Local::now().date_naive();
        let month = self.editor_month.get();
        let selected = self.editor_date.get();

        let first = NaiveDate::from_ymd_opt(month.year(), month.month(), 1).unwrap_or(today);
        let start = first - chrono::Duration::days(first.weekday().num_days_from_monday() as i64);

        let mut cells = Vec::with_capacity((CALENDAR_ROWS * CALENDAR_COLS) as usize);
        for i in 0..CALENDAR_ROWS * CALENDAR_COLS {
            let date = start + chrono::Duration::days(i);
            cells.push(PickerDay {
                date: date.format("%Y-%m-%d").to_string().into(),
                day: date.day() as i32,
                in_month: date.year() == month.year() && date.month() == month.month(),
                is_today: date == today,
                is_selected: date == selected,
                // day_info 的 rest 里已经把周末和法定假日都算进去了
                rest: calendar_info::day_info(date).rest,
            });
        }
        self.editor_days_model.set_vec(cells);

        let app = ui.global::<AppData>();
        app.set_editor_days(ModelRc::from(self.editor_days_model.clone()));
        app.set_editor_month_label(format!("{} 年 {} 月", month.year(), month.month()).into());
        // 内置的 DatePickerPopup 要的是 `Date { year, month, day }`,所以除了
        // 上面那堆文案之外,还得单独推一份拆成整数的
        app.set_editor_year(selected.year());
        app.set_editor_month_num(selected.month() as i32);
        app.set_editor_day(selected.day() as i32);
        app.set_editor_date_label(
            format!(
                "{} 月 {} 日 {}",
                selected.month(),
                selected.day(),
                model::weekday_name(selected.weekday())
            )
            .into(),
        );
    }


    // ── 滚轮的拖动与惯性滑动 ────────────────────────────────────────────

    /// 把滚轮的浮点位置推给 UI
    fn push_wheel_pos(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let app = ui.global::<AppData>();
        let slot = self.wheel.borrow();
        match slot.as_ref() {
            Some(w) if w.kind == 0 => app.set_hour_wheel_pos(w.pos),
            Some(w) => app.set_minute_wheel_pos(w.pos),
            None => {}
        }
    }

    fn wheel_pos_of(&self, kind: i32) -> f32 {
        if kind == 0 {
            self.editor_hour.get() as f32
        } else {
            self.editor_minute.get() as f32
        }
    }

    fn begin_wheel_drag(&self, kind: i32) {
        // 上一段滑行可能还在跑,先掐掉 —— 手一按下去就该听手的
        if let Some(t) = self.fling_timer.borrow().as_ref() {
            t.stop();
        }
        let pos = self.wheel_pos_of(kind);
        *self.wheel.borrow_mut() = Some(WheelDrag {
            kind,
            pos,
            velocity: 0.0,
            last_pos: pos,
            last_at: Instant::now(),
            dragging: true,
        });
    }

    fn drag_wheel_to(&self, kind: i32, pos: f32) {
        let now = Instant::now();
        let pos = pos.clamp(0.0, (wheel_count(kind) - 1) as f32);
        {
            let mut slot = self.wheel.borrow_mut();
            let Some(w) = slot.as_mut() else { return };
            if w.kind != kind {
                return;
            }
            // 速度 = 位置差分 / 时间差分。间隔太短的采样不算数(见 velocity_sample)。
            let dt = now.duration_since(w.last_at).as_secs_f32();
            let sampled = dt >= VELOCITY_MIN_DT;
            w.velocity = velocity_sample(w.velocity, w.last_pos, pos, dt);
            w.pos = pos; // 位置永远照常跟手,跟速度估计是否采信无关
            if sampled {
                w.last_pos = pos;
                w.last_at = now;
            }
        }
        self.push_wheel_pos();
    }

    /// 鼠标滚轮拨一格。
    ///
    /// **不走拖动那套**:拨一格就是「值 ±1」,跟位置、速度、惯性都没关系。
    /// 之前让它假扮成一次拖动,结果位置改归宿主管之后,少调一个 `drag-started`
    /// 就整个失灵(`drag_wheel_to` 找不到拖动状态直接 return)。
    fn nudge_wheel(&self, kind: i32, delta: i32) {
        // 滑行途中拨滚轮:先停住,并以「当前看到的位置」为基准,
        // 不然会从"已经被甩到但还没停稳"的那个值开始跳。
        let base = self
            .wheel
            .borrow()
            .as_ref()
            .filter(|w| w.kind == kind)
            .map(|w| w.pos.round() as i32)
            .unwrap_or_else(|| {
                if kind == 0 {
                    self.editor_hour.get()
                } else {
                    self.editor_minute.get()
                }
            });
        if let Some(t) = self.fling_timer.borrow().as_ref() {
            t.stop();
        }
        self.wheel.borrow_mut().take();

        let value = (base + delta).clamp(0, wheel_count(kind) - 1);
        if kind == 0 {
            self.set_editor_hour(value);
        } else {
            self.set_editor_minute(value);
        }
    }

    fn end_wheel_drag(&self, kind: i32) {
        let speed = {
            let mut slot = self.wheel.borrow_mut();
            let Some(w) = slot.as_mut() else { return };
            if w.kind != kind {
                return;
            }
            w.dragging = false;
            // 松手之前手指已经停了一会儿 → 这是"放",不是"甩"
            if Instant::now().duration_since(w.last_at) > DRAG_STALE {
                w.velocity = 0.0;
            }
            w.velocity.abs()
        };

        // 投影一下这一甩总共还能滑多远(闭式解 v₀/k)。
        // 不足半格就不必起定时器了 —— 反正吸附后落在同一格上,
        // 白跑十几帧还会让滚轮多抖一下。
        if speed < FLING_MIN_SPEED || fling_distance(speed) < 0.5 {
            // 轻放 / 点选 / 滚轮拨一格:没有惯性,直接吸附
            self.settle_wheel(kind);
        } else {
            self.start_fling();
        }
    }

    /// 启动惯性推进的定时器(已经在跑就重设,不影响什么)
    fn start_fling(&self) {
        let Some(me) = self.me.borrow().upgrade() else {
            return;
        };
        if let Some(t) = self.fling_timer.borrow().as_ref() {
            t.start(TimerMode::Repeated, FLING_TICK, move || me.tick_fling());
        }
    }

    /// 每 16ms 推进一步
    fn tick_fling(&self) {
        let dt = FLING_TICK.as_secs_f32();
        let mut settled = false;
        {
            let mut slot = self.wheel.borrow_mut();
            let Some(w) = slot.as_mut() else { return };
            if w.dragging {
                return;
            }
            w.velocity = fling_step(w.velocity, dt);
            w.pos += w.velocity * dt;

            // 撞到两端就停住,别让速度继续积(否则会在端点上"粘"一会儿)
            let max = (wheel_count(w.kind) - 1) as f32;
            if w.pos <= 0.0 {
                w.pos = 0.0;
                w.velocity = 0.0;
            } else if w.pos >= max {
                w.pos = max;
                w.velocity = 0.0;
            }

            if w.velocity.abs() < FLING_MIN_SPEED {
                settled = true;
            }
        }

        if settled {
            let kind = self.wheel.borrow().as_ref().map(|w| w.kind).unwrap_or(0);
            self.settle_wheel(kind);
        } else {
            self.push_wheel_pos();
        }
    }

    /// 收尾:吸附到最近一整格,报给宿主,并停掉定时器
    fn settle_wheel(&self, kind: i32) {
        let value = {
            let slot = self.wheel.borrow();
            let Some(w) = slot.as_ref() else { return };
            w.pos.round() as i32
        };
        // 先清状态再报值 —— set_editor_hour 会把位置一起推回去
        self.wheel.borrow_mut().take();
        if let Some(t) = self.fling_timer.borrow().as_ref() {
            t.stop();
        }
        // 位置由 push_editor_time 一并推回去(吸附后的整数格)
        if kind == 0 {
            self.set_editor_hour(value);
        } else {
            self.set_editor_minute(value);
        }
    }


    // ── 桌宠 ──────────────────────────────────────────────────────────

    /// 未完成的任务数(桌宠角标)
    fn remaining_count(&self) -> i32 {
        self.todos.borrow().iter().filter(|t| !t.done).count() as i32
    }

    /// 桌宠头顶气泡的文字。空 = 不显示。
    fn pet_bubble(&self) -> String {
        let ids = self.alerting.borrow();
        match ids.len() {
            0 => String::new(),
            1 => self
                .title_of(ids[0])
                .map(|t| format!("该做「{t}」了"))
                .unwrap_or_default(),
            n => format!("有 {n} 条提醒"),
        }
    }

    /// 把桌宠要的状态推给它**自己那份** AppData。
    ///
    /// 全局单例不跨窗口共享(见 widgets.slint 顶部),桌宠和设置窗口一样
    /// 各持一份 —— 漏推的表现是「桌宠一直停在加载时的样子」。
    fn push_pet(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        let app = pet.global::<AppData>();
        app.set_pet_size(self.pet_size.get());
        app.set_pet_count(self.remaining_count());
        app.set_pet_bubble(self.pet_bubble().into());
        app.set_pet_adding(self.pet_adding.get());
        self.push_pet_frame();
    }

    /// 只推当前这一帧(动画心跳每翻一帧调一次,别的状态不动)
    fn push_pet_frame(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        let (anim, frame) = self.pet_anim.get();
        if let Some(img) = self.pet_frames.frame(anim, frame) {
            pet.global::<AppData>().set_pet_frame(img);
        }
    }

    /// 动画心跳:累积时间,够一帧就翻。
    fn tick_pet(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        // 看不见就不烧 CPU(收进托盘、被别的窗口盖住时没必要动)
        if !pet.window().is_visible() {
            return;
        }
        let (anim, frame) = self.pet_anim.get();
        let count = self.pet_frames.frame_count(anim);
        if count == 0 {
            return;
        }
        let (next, left) = pet::advance(
            frame,
            count,
            self.pet_left_ms.get(),
            PET_TICK.as_millis() as f64,
            self.pet_frame_delay(anim),
        );
        self.pet_left_ms.set(left);

        // 转过一圈了(frame 回到开头):该考虑换动画了
        if next < frame {
            self.pet_finished_loop();
            // 换动画的那个分支自己会把画面推出去,这里只处理没换的情况
            if self.pet_anim.get().0 != anim {
                return;
            }
        }
        if next != frame {
            self.pet_anim.set((anim, next));
            self.push_pet_frame();
        }
    }

    /// 某个动画这一帧该停多久(毫秒)。速度倍率作用在**帧时长**上:2× 就是每帧等一半。
    ///
    /// 下限 1ms 兜底,免得倍率再大也除出 0(`pet::advance` 内部还会再 max(1) 一次)。
    fn pet_frame_delay(&self, anim: usize) -> u64 {
        let base = self.pet_frames.delay_ms(anim);
        let speed = self.pet_anim_speed(self.pet_frames.names().get(anim).copied().unwrap_or(""));
        (base as f64 / speed.max(0.01) as f64).round().max(1.0) as u64
    }

    /// 某个动画的速度倍率(没配过 = 1.0)
    fn pet_anim_speed(&self, name: &str) -> f32 {
        self.pet_speeds.borrow().get(name).copied().unwrap_or(model::PET_SPEED_DEFAULT)
    }

    /// 当前动画播完一圈了。
    ///
    /// - **交互动画**(看书/购物/星星…):只播一轮,然后回到待机轮播。
    ///   不这么做的话,「加完任务」那个购物动画会一直播下去。
    /// - **待机动画**:住够 `PET_IDLE_DWELL` 就随机换一个别的 ——
    ///   素材里有 8 个待机,不轮播的话另外 7 个永远见不到。
    fn pet_finished_loop(&self) {
        if !self.pet_idle.get() {
            self.rotate_pet_idle();
            return;
        }
        if Instant::now() < self.pet_idle_until.get() {
            return; // 还没住够,把这个待机再播一轮
        }
        self.rotate_pet_idle();
    }

    /// 随机换到另一个待机动画(不连着播同一个)。
    fn rotate_pet_idle(&self) {
        let all = self.pet_frames.idle_names();
        if all.is_empty() {
            return;
        }
        let current_name = self.pet_frames.names().get(self.pet_anim.get().0).copied();
        // 候选里排掉正在播的那个;只剩一个的话就还播它
        let others: Vec<&str> = all.iter().copied().filter(|n| Some(*n) != current_name).collect();
        let pool = if others.is_empty() { all } else { others };

        let mut seed = self.pet_rng.get();
        let pick = (next_rand(&mut seed) % pool.len() as u64) as usize;
        // 再摇一次当抖动,免得换动画的节奏像节拍器
        let jitter = next_rand(&mut seed) % PET_IDLE_JITTER_MS;
        self.pet_rng.set(seed);

        self.play_pet_anim(pool[pick]);
        self.pet_idle.set(true);
        self.pet_idle_until
            .set(Instant::now() + PET_IDLE_DWELL + Duration::from_millis(jitter));
    }

    /// 回到待机状态,并立刻换一个待机动画(不写死哪一个)。
    fn back_to_idle(&self) {
        self.pet_idle.set(true);
        self.pet_idle_until.set(Instant::now()); // 允许马上换
        self.rotate_pet_idle();
    }

    /// 播一个**交互动画**:播完一轮自动回到待机。
    ///
    /// 这是「动画效果太少」的正解 —— 素材里 10 个动画,交互专用的只有看书/购物,
    /// 其余靠待机轮播铺开(见 `rotate_pet_idle`)。
    fn play_pet_cue(&self, name: &str) {
        if self.pet_frames.index_of(name).is_none() {
            return; // 素材里没有就静默跳过(删了素材也不该崩)
        }
        self.pet_idle.set(false);
        self.play_pet_anim(name);
    }

    // ── 拖桌宠 ────────────────────────────────────────────────────────
    //
    // 没用 Slint 的 `WindowMoveArea`(它拿不到 Moved 事件,原因写在 ui/pet.slint 里),
    // 改成宿主自己算位移。这里有个**必须注意**的点:窗口是跟着指针走的,
    // 所以 Slint 给的 `mouse-x/mouse-y`(窗口内坐标)在拖动过程中几乎不变 ——
    // 用它算位移永远是 0。只能取系统指针的全局坐标。

    /// 按下:记下「指针的全局位置」和窗口此刻的位置,作为整个拖动的基准。
    ///
    /// 指针位置是用**按下点在窗口内的坐标 + 窗口位置**算出来的,不是当场轮询系统
    /// 指针 —— 事件被处理时指针可能已经又走了几像素,那样基准就偏了,一按下去
    /// 宠物会跳一下(实测能差 20px)。
    fn pet_drag_begin(&self, mouse_x: f32, mouse_y: f32) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        let pos = pet.window().position();
        let cursor0 = (pos.x + mouse_x.round() as i32, pos.y + mouse_y.round() as i32);
        self.pet_drag.set(Some((cursor0, (pos.x, pos.y))));
        self.pet_just_dragged.set(false);
    }

    /// 拖动中:按指针相对基准点的位移挪窗口。
    fn pet_drag_slide(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        let Some((cursor0, win0)) = self.pet_drag.get() else { return };
        let cursor = platform::cursor_pos();
        let (dx, dy) = (cursor.0 - cursor0.0, cursor.1 - cursor0.1);
        // 3px 以内当手抖,不算拖动 —— 否则单纯点一下也会把 clicked 吞掉
        if dx.abs() < 3 && dy.abs() < 3 {
            return;
        }
        self.pet_just_dragged.set(true);
        pet.window()
            .set_position(slint::PhysicalPosition::new(win0.0 + dx, win0.1 + dy));
    }

    /// 松开:清掉基准。`pet_just_dragged` 留着,给紧跟着的那个 `clicked` 判断。
    fn pet_drag_end(&self) {
        self.pet_drag.set(None);
    }

    /// 设置里调桌宠大小。夹进合法范围 → 重排窗口 → 存盘。
    fn set_pet_size(&self, value: f32) {
        let value = model::clamp_pet_size(value);
        if (value - self.pet_size.get()).abs() < 0.01 {
            return;
        }
        self.pet_size.set(value);
        // 窗口高度是「宠物 + 气泡/输入条」算出来的,所以要把 size 推给宠物窗口
        // 之后重排,否则宠物会溢出窗口。
        // 先推给宠物窗口,再重排:高度是「宠物 + 气泡/输入条」算出来的。
        // 不用手动清 pet_h —— 新高度和缓存值必然差得够多,layout 自己会重算,
        // 而且它拿缓存高度当「底边」,所以改大小是**从脚底往上长**,不会跳位置。
        self.push_pet();
        self.layout_pet_window();
        self.push_pet_settings();
        self.save();
    }

    /// 设置里调**某个**动画的速度。
    ///
    /// 注意这里**故意不把新列表推回设置窗口**:推回去会让 `for` 把那一行重建,
    /// 正在拖的滑杆当场被换掉,手感直接断(和当初「滚轮赋值后立刻读」同一类坑)。
    /// 滑杆自己已经显示着新值,下次打开设置时再按存的值重建一遍就够了。
    fn set_pet_anim_speed(&self, name: &str, value: f32) {
        let value = model::clamp_pet_speed(value);
        {
            let mut speeds = self.pet_speeds.borrow_mut();
            if speeds.get(name).is_some_and(|v| (v - value).abs() < 0.001) {
                return;
            }
            speeds.insert(name.to_string(), value);
        }
        self.save();
    }

    /// 把动画清单(name + 中文标签 + 当前速度)推给**设置窗口**。
    fn push_pet_anims(&self) {
        let Some(settings) = self.settings.borrow().clone() else { return };
        let anims: Vec<PetAnim> = self
            .pet_frames
            .names()
            .into_iter()
            .map(|name| PetAnim {
                name: name.into(),
                label: pet_anim_label(name).as_str().into(),
                speed: self.pet_anim_speed(name),
            })
            .collect();
        settings.global::<AppData>().set_pet_anims(ModelRc::from(Rc::new(VecModel::from(anims))));
    }

    /// 把桌宠参数推给**设置窗口**那份 AppData(滑杆要显示当前值)。
    ///
    /// 全局单例不跨窗口共享 —— 设置窗口和主窗口各持一份,所以两个窗口
    /// 都得接住回调、也都要推数据。漏推的表现是「重开设置窗口滑杆回到默认值」。
    fn push_pet_settings(&self) {
        let Some(settings) = self.settings.borrow().clone() else { return };
        let app = settings.global::<AppData>();
        app.set_pet_size(self.pet_size.get());
        self.push_pet_anims();
    }

    /// 换个动画播(从第 0 帧起)
    fn play_pet_anim(&self, name: &str) {
        if let Some(idx) = self.pet_frames.index_of(name) {
            self.pet_anim.set((idx, 0));
            self.pet_left_ms.set(0.0);
            self.push_pet_frame();
        }
    }

    /// 按当前内容算出窗口尺寸并摆好位置。
    ///
    /// **底边固定**:气泡/输入条出现时窗口往上长,宠物的脚不动。
    /// 不这么做的话,一点击宠物就会整体往上跳一下。
    fn layout_pet_window(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        let app = pet.global::<AppData>();

        // 高度 = 宠物 + 顶部给角标留的余量 + 气泡/输入条
        let pet_size = self.pet_size.get();
        let mut h = pet_size + 18.0;
        if !app.get_pet_bubble().is_empty() {
            h += 36.0;
        }
        if self.pet_adding.get() {
            h += 42.0;
        }
        let w = PET_WIDTH.max(pet_size + 24.0);

        // 高度没变就别动窗口:否则每次同步都 set_position,
        // 用户正拖着的时候会被拽回原处。
        let old_h = self.pet_h.get();
        if old_h > 0.0 && (h - old_h).abs() < 0.5 {
            return;
        }

        // 位置分两种:
        // - **第一次**:默认停在工作区右下角。这里必须把 x 也一起算,
        //   不能沿用窗口当时的 x —— 刚创建时它是 0,宠物会跑到屏幕最左边。
        // - **之后**:从窗口**当前位置**往上长。用当前位置而不是另记一个坐标,
        //   是为了让用户拖动之后,下次展开输入条仍然从他放的地方长出来。
        let first = old_h <= 0.0;
        let pos = pet.window().position();
        let (wx, wy, ww, wh) = platform::work_area();
        let x = if first {
            (wx + ww - w as i32 - PET_MARGIN) as f32
        } else {
            pos.x as f32
        };
        let bottom = if first {
            (wy + wh - PET_MARGIN) as f32
        } else {
            pos.y as f32 + old_h
        };
        self.pet_h.set(h);

        pet.window().set_size(slint::LogicalSize::new(w, h));
        pet.window()
            .set_position(slint::LogicalPosition::new(x, bottom - h));
    }

    /// 双击宠物:把主窗口叫出来
    fn show_main_window(&self) {
        if let Some(ui) = self.ui.upgrade() {
            let _ = ui.show();
        }
        platform::bring_to_front(platform::WINDOW_TITLE);
    }

    /// 点宠物:展开 / 收起快速添加
    fn toggle_pet_input(&self) {
        // 拖动松手时 TouchArea 也会发一次 clicked(松手时指针还在宠物上)。
        // 那不是"点一下",别顺手把输入条开了。
        if self.pet_just_dragged.get() {
            self.pet_just_dragged.set(false);
            return;
        }
        let open = !self.pet_adding.get();
        self.pet_adding.set(open);
        if !open {
            // 收起时把草稿清掉 —— 留着的话下次展开会看到上次没发出去的内容
            if let Some(pet) = self.pet.borrow().clone() {
                pet.global::<AppData>().set_pet_draft("".into());
            }
        }
        // 展开 = 看书;收起就回待机 —— 交给轮播挑一个,别写死 idle-1
        // (写死的话收起之后永远只播同一个,又变回「只有一个动画」了)
        if open {
            self.play_pet_cue("read");
        } else {
            self.back_to_idle();
        }
        self.layout_pet_window();
        if let Some(pet) = self.pet.borrow().clone() {
            pet.global::<AppData>().set_pet_adding(open);
        }
    }

    /// 从桌宠直接加一条任务(不带日期 —— 快速记一笔的场景)
    fn add_quick_task(&self, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.todos.borrow_mut().push(Todo {
            id,
            title: title.to_string(),
            done: false,
            due: None,
            remind_before: model::NO_REMIND,
            notified: false,
        });
        if let Some(pet) = self.pet.borrow().clone() {
            pet.global::<AppData>().set_pet_draft("".into());
        }
        // 记完一笔,让她高兴一下(购物 = 唯一的另一个非待机动画)
        self.play_pet_cue("shop");
        // 收起来,让位给下一次输入
        self.toggle_pet_input();
        self.sync();
    }

    /// 「今天」「明天」两个快捷键。走的是和点月历格子同一条路,
    /// 所以月份显示、选中态、标题都会一起跟上。
    fn set_editor_day(&self, offset: i32) {
        let date = Local::now().date_naive() + chrono::Duration::days(offset as i64);
        self.select_editor_date(date);
    }

    /// 选中某一天:改状态 → 把月历翻到那个月 → 重画。
    fn select_editor_date(&self, date: NaiveDate) {
        self.editor_date.set(date);
        self.editor_month
            .set(NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date));
        self.build_editor_days();
    }

    fn pick_editor_day(&self, date: &str) {
        if let Ok(date) = NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            self.select_editor_date(date);
        }
    }

    fn shift_editor_month(&self, delta: i32) {
        let month = self.editor_month.get();
        // 直接按「第几个月」算,让 chrono 处理跨年(负数也成立)
        let total = month.year() * 12 + (month.month() as i32 - 1) + delta;
        let year = total.div_euclid(12);
        let month_no = total.rem_euclid(12) as u32 + 1;
        if let Some(next) = NaiveDate::from_ymd_opt(year, month_no, 1) {
            self.editor_month.set(next);
            self.build_editor_days();
        }
    }

    /// 把当前选中的时刻推给 UI。
    ///
    /// `editor-hour` / `editor-minute` 在 AppData 里是 **in** 属性(不是 in-out),
    /// 也就是说 UI 只读、不会自己改 —— 宿主每改一次都得推一遍。
    /// 漏了这一步的表现是:点数字块毫无反应(选中态一动不动),
    /// 打开浮层时也永远停在 09:00,不管任务本来设的是几点。
    fn push_editor_time(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let app = ui.global::<AppData>();
        let (h, m) = (self.editor_hour.get(), self.editor_minute.get());
        app.set_editor_hour(h);
        app.set_editor_minute(m);
        // 内置选择器入口按钮上的 "09:30"。
        // 这里**不看「全天」** —— 全天是 UI 自己的 in-out 属性(Toggle 直接绑着),
        // 宿主不会收到通知,拿它做判断的话标签会一直是旧值。按钮在「全天」下本来就是
        // 置灰的,灰着的时刻不会让人误解。
        app.set_editor_time_label(format!("{h:02}:{m:02}").into());
        // 滚轮的浮点位置也跟着摆正。放在这里而不是每次滑动后单独推,
        // 是为了让「值」和「位置」只有一个出口,不会各说各话。
        app.set_hour_wheel_pos(h as f32);
        app.set_minute_wheel_pos(m as f32);
    }

    fn set_editor_hour(&self, hour: i32) {
        self.editor_hour.set(hour.clamp(0, 23));
        self.push_editor_time();
    }

    fn set_editor_minute(&self, minute: i32) {
        self.editor_minute.set(minute.clamp(0, 59));
        self.push_editor_time();
    }

    /// 保存时间设置。
    ///
    /// 日期和时刻现在都是**选择器点出来的**,不可能非法,所以这一段没有解析、
    /// 也没有「格式不对」的报错分支了 —— 需要报错的那条路在源头就被堵掉了。
    fn save_time_editor(&self) {
        let id = self.editor.get();
        if id < 0 {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let app = ui.global::<AppData>();
        let all_day = app.get_editor_all_day();
        let remind_index = app.get_editor_remind();
        let date = self.editor_date.get();

        let due = due_from_picker(
            date,
            self.editor_hour.get(),
            self.editor_minute.get(),
            all_day,
        );

        if let Some(mut todo) = self.find_mut(|t| t.id == id) {
            todo.due = Some(due);
            todo.remind_before = remind_minutes(remind_index);
            todo.reset_notified(); // 改了时间就重新计时
        }
        self.editor.set(-1);
        app.set_editor_id(-1);
        self.sync();
    }

    fn clear_due(&self) {
        let id = self.editor.get();
        if id < 0 {
            return;
        }
        if let Some(mut todo) = self.find_mut(|t| t.id == id) {
            todo.due = None;
        }
        self.close_time_editor();
        self.sync();
    }

    // ── 提醒 ──────────────────────────────────────────────────────────

    /// 扫一遍提醒。到点的弹系统通知,并记住哪些还没被确认。
    fn tick_reminders(&self) {
        let now = Local::now();
        let mut fired = {
            let mut todos = self.todos.borrow_mut();
            reminder::scan(&mut todos, now)
        };
        if fired.is_empty() {
            return;
        }

        if !fired.fresh.is_empty() {
            let (title, body) = reminder::notify_content(&fired, now);
            platform::notify(&title, &body);
            // 到点了就换一个辨识度最高的动画(星星那套),别让它静悄悄地飘过
            self.play_pet_cue("idle-4");
            for todo in &fired.fresh {
                self.alerting.borrow_mut().push(todo.id);
            }
        }
        fired.fresh.clear();
        self.sync(); // 顺带把 notified 落盘、横幅刷新
    }

    fn acknowledge_reminders(&self) {
        self.alerting.borrow_mut().clear();
        self.sync();
    }

    // ── 设置 ──────────────────────────────────────────────────────────

    fn set_autostart(&self, enabled: bool) {
        match platform::set_autostart(enabled) {
            Ok(()) => {
                if let Some(ui) = self.ui.upgrade() {
                    ui.global::<AppData>().set_autostart(platform::autostart_enabled());
                }
            }
            Err(err) => {
                platform::log(&format!("设置开机自启失败: {err}"));
                if let Some(ui) = self.ui.upgrade() {
                    // 写失败就把开关拨回去
                    ui.global::<AppData>().set_autostart(platform::autostart_enabled());
                }
            }
        }
    }

    /// 窗口尺寸变了就调整日历的详略。
    ///
    /// 为什么要轮询而不是绑定:Slint 里 `Window.height` 和布局的首选高度
    /// 是互相牵扯的,在 .slint 里写 `self.height < 640px` 会直接构成绑定环。
    /// 从 Rust 查真实窗口尺寸不参与绑定图,最多晚 300ms 生效。
    fn poll_window(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let window = ui.window();
        let logical_h = window.size().height as f32 / window.scale_factor();
        let compact = logical_h < 640.0;
        if compact != self.compact.get() {
            self.compact.set(compact);
            ui.global::<AppData>().set_compact_cells(compact);
        }
        // 最大化状态也一样:拖到屏幕顶部、Win+Up 这些是系统干的,
        // 看实际窗口矩形才知道标题栏按钮该画哪个图标
        ui.global::<AppData>().set_maximized(platform::window_fills_work_area());

        // 窗口圆角**跟着外观走**:毛玻璃给圆角,原主题给直角(恢复改造前的样子)。
        // 要拿到窗口句柄才能设,所以挂在轮询里 —— 内部只在值真的变了时才调 DWM。
        platform::set_main_rounding(self.appearance.get() == AppearanceName::Glass);

        // 设置窗口的任务栏样式补丁(原因见 open_settings)
        if self.settings_fixup_pending.get() {
            self.settings_fixup_pending.set(false);
            platform::hide_from_taskbar(platform::SETTINGS_TITLE);
        }
        // 桌宠同理
        if self.pet_fixup_pending.get() {
            self.pet_fixup_pending.set(false);
            platform::hide_from_taskbar(platform::PET_TITLE);
        }

        // 周视图每格高度:窗口高度减去上面那些固定条(标题栏/头部/切换/输入框/
        // 星期条/底栏,约 284px)再 7 等分。
        let area = (logical_h - 284.0).max(46.0 * 7.0 + 24.0);
        let card = ((area - 24.0) / 7.0).clamp(46.0, 220.0);
        ui.global::<AppData>().set_week_card_height(card);
    }

    // ── 托盘 ──────────────────────────────────────────────────────────

    fn poll_tray(&self) {
        let action = self
            .tray
            .borrow()
            .as_ref()
            .and_then(|tray| tray.poll());
        let Some(action) = action else { return };
        let Some(ui) = self.ui.upgrade() else { return };

        match action {
            TrayAction::Open => {
                let _ = self.reveal(&ui);
            }
            TrayAction::Today => {
                let _ = self.reveal(&ui);
                self.set_view(View::Calendar);
                self.goto_today();
            }
            TrayAction::Settings => {
                self.open_settings();
            }
            TrayAction::TogglePet(on) => {
                self.show_pet.set(on);
                if let Some(pet) = self.pet.borrow().clone() {
                    if on {
                        self.layout_pet_window();
                        let _ = self.reveal(&*pet);
                        platform::hide_from_taskbar(platform::PET_TITLE);
                        self.pet_fixup_pending.set(true);
                    } else {
                        let _ = pet.hide();
                    }
                }
                self.save();
            }
            TrayAction::Quit => {
                self.save();
                slint::quit_event_loop().ok();
            }
        }
    }
}

// ── 入口 ──────────────────────────────────────────────────────────────

/// 显示一个窗口,并且**在它真正映射到屏幕之后**再要一次重画。
///
/// 每个窗口都必须走这里,别直接 `show()` —— 直接 show 会得到一个「只画了一部分」
/// 的窗口,而且只有本进程会这样。原因:
///
/// 1. Slint 会在窗口**还没映射**的时候先把第一帧渲染好(X11 上防露出未初始化显存,
///    见 `set_visibility` 里那段 `Pre-render the first frame before mapping`)。
/// 2. 这一帧经 softbuffer 呈现之后,缓冲区的 age 就变成 1,Slint 据此认为
///    「缓冲区里还有上一帧」,后续只重画「脏」的那几块。
/// 3. 但窗口映射出来时,系统给的是一块**全新的、全透明的**画面 —— 上一帧并不在。
/// 4. 于是整窗口只有脏区那几块有内容,其余地方 alpha 为 0。
///
/// 前三步在别的渲染器上看不出来(GL 每帧整屏重画,femtovg/skia-GL 的 surface
/// 也不带 alpha)。本进程为了桌宠必须用**带 alpha 通道的软件渲染器**,于是那些
/// 「没画到」的地方就直接透出桌面 —— 实测启动后整个主窗口只有输入框和筛选标签
/// 两小块有内容,其余全是桌面。
///
/// 修法:**让缓冲区的尺寸变一次**。softbuffer 的缓冲区只在尺寸变化时重新分配,
/// 而新缓冲区没「呈现」过,age 就是 0 —— Slint 据此判定必须整窗口重画,
/// 顺带整窗口 blit 一次,空白处一次性补齐。
///
/// 两条走过的弯路,别再试:
/// - **`request_redraw()` 不行**。窗口映射后树是干净的,空脏区确实会整窗口重画,
///   但 `show()` 是在事件循环跑起来之前调的,winit 窗口那时还没建出来,
///   `set_visibility` 里还会把挂起的重画请求清掉 —— 请求被吞了。
/// - **两次改尺寸不能挤在同一个 tick 里**。净变化为零,等于没改。
///   中间必须让事件循环跑过一轮(这里留 120ms)。
///
/// 视觉上不可见:只是高度顶 1px 又收回来,而且发生在窗口刚出现的那一瞬间。
impl State {
    fn reveal<W: slint::ComponentHandle + 'static>(&self, ui: &W) -> Result<(), slint::PlatformError> {
        let weak = ui.as_weak();
        ui.show()?;

        // 换算成逻辑尺寸再改:set_size 收 LogicalSize,而 Window::size() 给的是物理的,
        // 直接拿物理数当逻辑用会在高 DPI 上把窗口缩小。
        let scale = ui.window().scale_factor();
        let size = ui.window().size().to_logical(scale);
        ui.window().set_size(slint::LogicalSize::new(size.width, size.height + 1.0));

        let timer = Timer::default();
        timer.start(TimerMode::SingleShot, Duration::from_millis(120), move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_size(size);
            }
        });
        // Timer drop 掉就停了,必须留个引用
        self.repaint_timers.borrow_mut().push(timer);
        Ok(())
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // 编进了 Skia 就用 Skia 当默认渲染器(文字最锐);
    // 不想到处加 SLINT_BACKEND 环境变量,所以在进程里设一次。
    // SAFETY: 这是在创建任何窗口和线程之前、main 的最开头设置的,没有并发读。
    // 默认用 **Skia 的软件后端**。
    //
    // 为什么不是纯 GL 的 `winit-skia`(更快):**桌宠要透明**。
    // 透明要求渲染面带 alpha 通道,而 Slint 的 GL surface 根本不申请 ——
    // `i-slint-renderer-skia` 里 `Surface::set_transparent` 是 trait 空实现,
    // 只有软件后端那条路真正保留 alpha。用 GL 渲染器的话桌宠会是一块黑方块。
    //
    // 选 `winit-skia-software` 而不是纯 `winit-software`:两者都能透明,
    // 但这条仍然走 Skia 的光栅化,文字质量和 GL 版一致(GUI 应用最怕小字发虚)。
    //
    // 仍然尊重外部设置,方便临时换渲染器对比。
    #[cfg(feature = "skia")]
    if std::env::var_os("SLINT_BACKEND").is_none() {
        unsafe {
            std::env::set_var("SLINT_BACKEND", "winit-skia-software");
        }
    }

    platform::rotate_log_if_needed();
    platform::log("---- 启动 ----");

    // 已经开着一个就直接把它叫到前台,别开第二个(否则双份提醒 + 双写文件)
    if !platform::acquire_single_instance() {
        platform::focus_existing_window();
        return Ok(());
    }

    let ui = AppWindow::new()?;
    let path = model::data_file();
    let data = model::load(&path);
    let next_id = data.todos.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    let today = Local::now().date_naive();

    let state = Rc::new(State {
        ui: ui.as_weak(),
        todos: RefCell::new(data.todos),
        list_model: Rc::new(VecModel::default()),
        days_model: Rc::new(VecModel::default()),
        day_model: Rc::new(VecModel::default()),
        week_model: Rc::new(VecModel::default()),
        filter: Cell::new(Filter::All),
        view: Cell::new(View::List),
        editing: Cell::new(-1),
        editor: Cell::new(-1),
        editor_date: Cell::new(today),
        editor_month: Cell::new(NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap()),
        editor_hour: Cell::new(9),
        editor_minute: Cell::new(0),
        wheel: RefCell::new(None),
        fling_timer: RefCell::new(None),
        me: RefCell::new(std::rc::Weak::new()),
        editor_days_model: Rc::new(VecModel::default()),
        editor_hours_model: Rc::new(VecModel::default()),
        editor_minutes_model: Rc::new(VecModel::default()),
        cursor: Cell::new(today),
        month: Cell::new(NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap()),
        alerting: RefCell::new(Vec::new()),
        next_id: Cell::new(next_id),
        path,
        tray: RefCell::new(None),
        compact: Cell::new(false),
        appearance: Cell::new(data.appearance),
        settings_fixup_pending: Cell::new(false),
        settings: RefCell::new(None),
        pet: RefCell::new(None),
        pet_frames: pet::PetFrames::load(),
        pet_anim: Cell::new((0, 0)),
        pet_left_ms: Cell::new(0.0),
        pet_timer: RefCell::new(None),
        pet_adding: Cell::new(false),
        pet_fixup_pending: Cell::new(false),
        show_pet: Cell::new(data.show_pet),
        pet_drag: Cell::new(None),
        pet_just_dragged: Cell::new(false),
        pet_size: Cell::new(model::clamp_pet_size(data.pet_size)),
        pet_speeds: RefCell::new(
            data.pet_speeds
                .iter()
                .map(|(k, v)| (k.clone(), model::clamp_pet_speed(*v)))
                .collect(),
        ),
        pet_idle: Cell::new(true),
        pet_idle_until: Cell::new(Instant::now()),
        pet_rng: Cell::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15)
                | 1, // 种子不能是 0
        ),
        pet_h: Cell::new(0.0),
        repaint_timers: RefCell::new(Vec::new()),
    });

    // 滚轮惯性用的自引用 + 定时器。定时器**建好但不启动** ——
    // 只有松手甩出去时才在 end_wheel_drag 里 start,滑停了就 stop,
    // 平时一个 tick 都不跑(不然 60Hz 空转会把进程一直吊着不睡)。
    state.me.replace(Rc::downgrade(&state));
    *state.fling_timer.borrow_mut() = Some(Timer::default());

    {
        let app = ui.global::<AppData>();
        app.set_todos(ModelRc::from(state.list_model.clone()));
        app.set_days(ModelRc::from(state.days_model.clone()));
        app.set_day_todos(ModelRc::from(state.day_model.clone()));
        // 时刻滚轮的候选:时 0-23、**分 0-59(一整分钟一档)**。
        // 两个都是固定的,推一次就够。
        //
        // 早先是 5 分钟一档,还专门写了段逻辑把历史数据里非整档的分钟
        // (比如手输时代留下的 09:37)补进候选里,免得用户的时间"选不中"。
        // 换成滚轮之后一档一分钟,0-59 全覆盖,那段补丁连同它的理由一起删了 ——
        // 候选表能表达全部合法值,就不存在"表示不了"的问题。
        state.editor_hours_model.set_vec(time_options(0..24));
        state.editor_minutes_model.set_vec(time_options(0..60));
        app.set_editor_days(ModelRc::from(state.editor_days_model.clone()));
        app.set_editor_hours(ModelRc::from(state.editor_hours_model.clone()));
        app.set_editor_minutes(ModelRc::from(state.editor_minutes_model.clone()));
        // 两套选择器用哪一套,由 USE_NATIVE_PICKERS 这个常量决定
        app.set_use_native_pickers(USE_NATIVE_PICKERS);
        app.set_week_days(ModelRc::from(state.week_model.clone()));
        app.set_autostart(platform::autostart_enabled());
    }
    ui.global::<Theme>().set_preference(theme_to_ui(data.theme));

    // ── 把 UI 回调接到 State 上 ──
    {
        let s = state.clone();
        ui.global::<Logic>().on_add_task(move |title| s.add(&title));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_toggle_task(move |id| s.toggle(id));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_remove_task(move |id| s.remove(id));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_toggle_all(move || s.toggle_all());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_clear_completed(move || s.clear_completed());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_set_filter(move |filter| s.set_filter(filter));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_begin_edit(move |id| s.begin_edit(id));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_commit_edit(move |text| s.commit_edit(&text));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_cancel_edit(move || s.set_editing(-1));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_cycle_theme(move || s.cycle_theme());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_set_view(move |view| s.set_view(view));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_shift_month(move |delta| s.shift_month(delta));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_goto_today(move || s.goto_today());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_shift_week(move |delta| s.shift_week(delta));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_select_day(move |date| s.select_day(&date));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_toggle_day_task(move |id| s.toggle(id));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_open_time_editor(move |id| s.open_time_editor(id));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_close_time_editor(move || s.close_time_editor());
    }
    {
        let s = state.clone();
        // 不用传参:日期/时刻宿主自己持有,「全天」和「提醒」它回读 UI 上的值
        ui.global::<Logic>().on_save_time_editor(move || s.save_time_editor());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_clear_due(move || s.clear_due());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_set_editor_day(move |offset| s.set_editor_day(offset));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_shift_editor_month(move |delta| s.shift_editor_month(delta));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_begin_wheel_drag(move |kind| s.begin_wheel_drag(kind));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_drag_wheel_to(move |kind, pos| s.drag_wheel_to(kind, pos));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_end_wheel_drag(move |kind| s.end_wheel_drag(kind));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_nudge_wheel(move |kind, delta| s.nudge_wheel(kind, delta));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_pick_editor_day(move |date| s.pick_editor_day(&date));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_set_editor_hour(move |hour| s.set_editor_hour(hour));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_set_editor_minute(move |minute| s.set_editor_minute(minute));
    }
    {
        let s = state.clone();
        // 内置选择器一次给出年月日;日子不合法就整个忽略,别把 state 弄坏
        ui.global::<Logic>().on_set_editor_date_parts(move |y, m, d| {
            if let Some(date) = NaiveDate::from_ymd_opt(y, m as u32, d as u32) {
                s.select_editor_date(date);
            }
        });
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_acknowledge_reminders(move || s.acknowledge_reminders());
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_set_autostart(move |on| s.set_autostart(on));
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_open_settings(move || s.open_settings());
    }
    // ── 无边框窗口的标题栏 ──
    ui.global::<Logic>().on_minimize_window(platform::minimize_window);
    {
        let s = state.clone();
        ui.global::<Logic>().on_toggle_maximize(move || {
            platform::toggle_maximize();
            s.poll_window(); // 立刻刷新按钮图标,不等下一次轮询
        });
    }
    {
        let s = state.clone();
        ui.global::<Logic>().on_hide_window(move || {
            // 主窗口都收进托盘了,还留个设置面板飘在桌面上会很突兀,一起收掉
            s.close_settings();
            if let Some(ui) = s.ui.upgrade() {
                let _ = ui.hide(); // 和点系统关闭按钮一样:收进托盘
            }
        });
    }

    // ── 设置窗口 ──
    // 启动时就建好、一直藏着,用的时候直接 show。这样做的原因:
    // 建窗口时要给它的回调捕获 `Rc<State>`,而 Rc 只有在 main 里才拿得到 ——
    // 放进 State 的方法里就得给 State 塞一个自引用的 Weak,不值当。
    // 顺带第一次打开也没有构造延迟。
    match SettingsWindow::new() {
        Ok(settings) => {
            {
                let s = state.clone();
                settings.global::<Logic>().on_set_appearance(move |value| s.set_appearance(value));
            }
            {
                let s = state.clone();
                settings.global::<Logic>().on_set_pet_size(move |v| s.set_pet_size(v));
            }
            {
                let s = state.clone();
                settings.global::<Logic>()
                    .on_set_pet_anim_speed(move |name, v| s.set_pet_anim_speed(&name, v));
            }
            {
                let s = state.clone();
                settings.global::<Logic>().on_close_settings(move || s.close_settings());
            }
            // 设置窗口自己也可能被关掉(点它的 ✕,或者 Alt+F4)
            settings.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);
            *state.settings.borrow_mut() = Some(Rc::new(settings));
        }
        Err(err) => {
            // 建不出来不影响主功能,只是设置入口点了没反应 —— 别让它拖垮整个启动
            platform::log(&format!("创建设置窗口失败,设置入口将不可用: {err}"));
        }
    }

    // ── 桌宠窗口 ──
    // 和设置窗口一样:独立窗口,有自己的 AppData/Logic/Theme,回调要单独接。
    if state.pet_frames.is_empty() {
        platform::log("assets/pet 里没有素材,桌宠不启动(先跑 tools/pet-assets)");
    } else {
        match PetWindow::new() {
            Ok(pet) => {
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_toggle_pet_input(move || s.toggle_pet_input());
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_add_quick_task(move |title| s.add_quick_task(&title));
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_show_main_window(move || s.show_main_window());
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_acknowledge_reminders(move || s.acknowledge_reminders());
                }
                // 拖动桌宠。宿主自己算位移 —— 没法用 WindowMoveArea,理由见 ui/pet.slint
                {
                    let s = state.clone();
                    pet.global::<Logic>()
                        .on_pet_drag_begin(move |x, y| s.pet_drag_begin(x, y));
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_pet_drag_slide(move || s.pet_drag_slide());
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_pet_drag_end(move || s.pet_drag_end());
                }
                // 桌宠没有「关掉」的语义(它是常驻挂件),被关就藏起来
                pet.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);
                *state.pet.borrow_mut() = Some(Rc::new(pet));
            }
            Err(err) => platform::log(&format!("创建桌宠窗口失败: {err}")),
        }

        // 动画心跳。固定细粒度、自己累积时间(见 pet::advance 的说明)。
        {
            let s = state.clone();
            let timer = Timer::default();
            timer.start(TimerMode::Repeated, PET_TICK, move || s.tick_pet());
            *state.pet_timer.borrow_mut() = Some(timer);
        }
    }

    // 主题(亮暗 + 外观)推给**所有**窗口。必须放在设置窗口建好之后。
    state.push_theme();

    // 关窗口只是收进托盘,不退出进程
    ui.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    // 托盘:失败也不影响主功能,只是关窗后就没法叫回来了
    match platform::Tray::new(state.show_pet.get()) {
        Ok(tray) => *state.tray.borrow_mut() = Some(tray),
        Err(err) => platform::log(&format!("创建托盘失败(关窗后将无法从托盘打开): {err}")),
    }

    state.sync(); // 把读到的数据渲染出来
    state.tick_reminders(); // 启动时补一次错过的提醒

    // 注意:Timer 必须绑到会活到事件循环结束的变量上。
    // 写成 `Timer::default().start(...)` 的话,Timer 在语句结束就被 drop,
    // 而 drop 会把它停掉 —— 表现就是定时器永远不触发,提醒和托盘都没反应。
    let tray_timer = Timer::default();
    {
        let s = state.clone();
        tray_timer.start(TimerMode::Repeated, TRAY_TICK, move || {
            s.poll_tray();
            s.poll_window();
        });
    }
    let reminder_timer = Timer::default();
    {
        let s = state.clone();
        reminder_timer.start(TimerMode::Repeated, REMINDER_TICK, move || s.tick_reminders());
    }

    // 关键:这里必须用 run_event_loop_until_quit,不能用 ui.run()。
    // ui.run() 在「没有可见窗口」时会自己结束事件循环,而「关窗口」只是把窗口
    // 藏起来 —— 托盘图标是 tray-icon crate 建的,Slint 并不知道它的存在,所以
    // 用 ui.run() 的话关窗就等于退出进程,托盘常驻就废了。
    // 退出的唯一入口是托盘菜单里的「退出」。
    state.reveal(&ui)?;

    // 桌宠:先摆好位置再显示,免得先在屏幕中间闪一下
    if let Some(pet) = state.pet.borrow().clone().filter(|_| state.show_pet.get()) {
        state.push_pet();
        state.layout_pet_window();
        if let Err(err) = state.reveal(&*pet) {
            platform::log(&format!("显示桌宠窗口失败: {err}"));
        }
        // 挂件不该占任务栏格子;和设置窗口同样的道理 —— winit 之后还会写窗口属性,
        // 所以这里先试一次,下一轮轮询再补(见 poll_window)
        platform::hide_from_taskbar(platform::PET_TITLE);
        state.pet_fixup_pending.set(true);
    }

    slint::run_event_loop_until_quit()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 时刻选择器里选出来的时/分,必须原样落进存盘结构 ——
    /// 这条路原来只能靠点界面来验,拎成纯函数之后就能钉住了。
    #[test]
    fn picker_time_round_trips() {
        let due = due_from_picker(NaiveDate::from_ymd_opt(2026, 9, 16).unwrap(), 14, 30, false);
        assert_eq!(due.date, "2026-09-16");
        assert_eq!(due.time.as_deref(), Some("14:30"));
        assert!(!due.is_all_day());
    }

    /// 时刻为 0 点时不能退化成「没有时刻」——
    /// 00:00 是个合法时刻,不是空值(拿 `is_empty()` 之类的判空很容易栽在这)。
    #[test]
    fn midnight_is_a_real_time() {
        let due = due_from_picker(NaiveDate::from_ymd_opt(2026, 9, 16).unwrap(), 0, 0, false);
        assert_eq!(due.time.as_deref(), Some("00:00"));
        assert!(!due.is_all_day(), "00:00 不能被当成全天");
    }

    /// 勾了「全天」就**不存时刻**。
    /// 存了的话会变成「用户设了 09:00」,而全天任务的 09:00 是提醒逻辑定的,
    /// 两件事混在一起以后就没法区分了。
    #[test]
    fn all_day_drops_the_time() {
        let due = due_from_picker(NaiveDate::from_ymd_opt(2026, 9, 16).unwrap(), 14, 30, true);
        assert_eq!(due.time, None);
        assert!(due.is_all_day());
    }

    /// 越界的时分兜底成 09:00,而不是 panic 或者存个非法值
    #[test]
    fn out_of_range_falls_back() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        assert_eq!(
            due_from_picker(date, 25, 0, false).time.as_deref(),
            Some("09:00")
        );
        assert_eq!(
            due_from_picker(date, 10, 99, false).time.as_deref(),
            Some("09:00")
        );
    }

    /// "09:30" 能被重新读回来(打开浮层时要拿它去点亮对应的格子)
    #[test]
    fn hhmm_parses_for_reopening() {
        assert_eq!(parse_hhmm("09:30"), Some((9, 30)));
        assert_eq!(parse_hhmm("00:00"), Some((0, 0)));
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("乱写的"), None);
    }

    /// 时刻候选必须是补零的两位字符串 —— Slint 那边没法补零,
    /// 漏了这一步界面上就会出现「5」和「15」宽度不一的一排方块。
    #[test]
    fn time_option_labels_are_zero_padded() {
        let opts = time_options([0, 5, 23]);
        let labels: Vec<&str> = opts.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["00", "05", "23"]);
        assert_eq!(opts[2].value, 23);
    }
}

#[cfg(test)]
mod fling_tests {
    use super::*;

    /// 速度按时间衰减,而且**单调递减**——不能出现越滑越快
    #[test]
    fn friction_only_slows_down() {
        let mut v = 30.0f32;
        let mut prev = v;
        for _ in 0..120 {
            v = fling_step(v, FLING_TICK.as_secs_f32());
            assert!(v < prev, "衰减过程中速度必须一直在掉: {prev} -> {v}");
            assert!(v > 0.0, "指数衰减永远不会到 0(所以才需要阈值收尾)");
            prev = v;
        }
    }

    /// 「一秒后保留 1%」这个定义要成立
    #[test]
    fn retain_ratio_holds_over_one_second() {
        // 分 100 步推进 1 秒,累积起来应当约等于一步推进 1 秒
        let v0 = 10.0f32;
        let mut stepwise = v0;
        for _ in 0..100 {
            stepwise = fling_step(stepwise, 0.01);
        }
        let oneshot = fling_step(v0, 1.0);
        assert!(
            (stepwise - oneshot).abs() / oneshot < 0.01,
            "逐步推进 {stepwise} 和一次推进 {oneshot} 应当基本一致(指数衰减可分步)"
        );
        assert!((oneshot - v0 * FLING_RETAIN_PER_SEC).abs() < 0.01, "一秒后应保留 {}%", FLING_RETAIN_PER_SEC * 100.0);
    }

    /// 逐帧积分的实际滑行距离,应当等于「闭式解**减去被阈值截掉的那条尾巴**」:
    ///
    ///     x = (v₀ − v_end) / k
    ///
    /// 直接拿 v₀/k 来比是错的 —— 那是滑到无穷远的极限,而运行时速度掉到阈值就停了。
    /// (第一版测试就是这么比的,小速度下差了 19%。错的是测试不是算法。)
    #[test]
    fn stepped_distance_matches_truncated_closed_form() {
        let dt = FLING_TICK.as_secs_f32();
        for v0 in [5.0f32, 20.0, 60.0] {
            let mut v = v0;
            let mut x = 0.0f32;
            while v.abs() >= FLING_MIN_SPEED {
                v = fling_step(v, dt);
                x += v * dt;
            }
            // 用真正停下来的那个速度当尾巴
            let expected = (v0 - v.abs()) / fling_k();
            let err = (x - expected).abs() / expected;
            assert!(
                err < 0.05,
                "v₀={v0}: 逐帧积分 {x:.3} 格,截断闭式解 {expected:.3} 格,差 {:.1}%",
                err * 100.0
            );
            // 而且永远不可能滑得比无穷远极限还远
            assert!(x <= fling_distance(v0), "v₀={v0}: 超出无穷远极限了");
        }
    }

    /// 慢速时被阈值截掉的尾巴占比很大,所以「投影滑行距离不足半格就别起定时器」
    /// 这个短路是必要的
    #[test]
    fn slow_flicks_get_absorbed_by_the_threshold() {
        assert!(fling_distance(1.0) < 0.5, "1 格/秒的一甩应当不足以滑过半格");
        assert!(fling_distance(30.0) > 3.0, "30 格/秒应当能滑出好几格");
    }

    /// 甩得越快滑得越远(单调),而且量级要合理 —— 一个 24 格的轮子,
    /// 轻轻一甩不该直接冲到顶,用力甩才有长距离。
    #[test]
    fn faster_flick_travels_farther() {
        let slow = fling_distance(5.0);
        let fast = fling_distance(30.0);
        assert!(fast > slow);
        assert!(slow > 0.5, "5 格/秒的一甩至少也该滑过半格: {slow}");
        assert!(fast < 24.0, "30 格/秒不该一下滑完整个 24 格的轮子: {fast}");
    }

    /// 阈值那一下必须真的收得住,不能永远在阈值边缘抖
    #[test]
    fn settles_below_threshold() {
        let mut v = FLING_MIN_SPEED * 1.2;
        let dt = FLING_TICK.as_secs_f32();
        let mut steps = 0;
        while v.abs() >= FLING_MIN_SPEED && steps < 1000 {
            v = fling_step(v, dt);
            steps += 1;
        }
        assert!(steps < 20, "从阈值附近收敛应当很快,实际用了 {steps} 帧");
    }

    /// **同一帧里的第二个 move 不能拿来做速度估计。**
    /// 这正是"怎么拖都会被甩到底"的元凶:dt 接近 0 而位移是真的,
    /// Δ/dt 能算出几千格/秒。
    #[test]
    fn same_frame_moves_do_not_explode_velocity() {
        // 0.5ms 内跳了 3 格 —— 按 Δ/dt 算是 6000 格/秒
        let v = velocity_sample(0.0, 0.0, 3.0, 0.0005);
        assert_eq!(v, 0.0, "间隔过短的采样必须被丢弃,而不是当成 6000 格/秒");

        // 已经有一个正常速度时,短间隔采样也不该把它带跑偏
        let kept = velocity_sample(12.0, 0.0, 3.0, 0.0005);
        assert_eq!(kept, 12.0, "丢弃采样时应当原样保留上一次的速度");
    }

    /// 正常间隔的采样:按平滑系数混进去
    #[test]
    fn normal_samples_blend_in() {
        // 0.2 秒走了 3 格 = 15 格/秒;旧速度 0 → 0.65 × 15
        let v = velocity_sample(0.0, 0.0, 3.0, 0.2);
        assert!((v - 15.0 * (1.0 - VELOCITY_SMOOTHING)).abs() < 0.01, "实际 {v}");
    }

    /// 再离谱的采样也有上限兜底
    #[test]
    fn velocity_is_clamped() {
        let v = velocity_sample(0.0, 0.0, 100.0, 0.05); // 原始算出来 2000 格/秒
        assert_eq!(v, VELOCITY_MAX, "超过上限应当被夹住");
        let slow = velocity_sample(0.0, 0.0, -100.0, 0.05);
        assert_eq!(slow, -VELOCITY_MAX, "反向也要夹");
    }

    /// 上限本身要合理:夹到上限之后滑行距离不该离谱到把整个轮子跑完
    #[test]
    fn clamped_velocity_is_still_sane() {
        let max_glide = fling_distance(VELOCITY_MAX);
        assert!(
            max_glide < 10.0,
            "就算速度到顶,一次滑行也不该滑过轮子的一半(24 格的一半是 12,实际 {max_glide} 格)"
        );
    }

    /// 格子数:时 24、分 60
    #[test]
    fn wheel_counts() {
        assert_eq!(wheel_count(0), 24);
        assert_eq!(wheel_count(1), 60);
    }
}
