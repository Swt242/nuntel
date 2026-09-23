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

mod ai;
mod calendar_info;
mod highlight;
mod markdown;
mod model;
mod notes;
mod pet;
mod platform;
mod reminder;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use chrono::{Datelike, Local, NaiveDate, NaiveTime, Timelike};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

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

/// 悬停时那一圈功能图标的几何。
///
/// **这几个数只有宿主知道** —— 桌宠窗口的大小就是按它们算出来的,所以 Slint
/// 那边只消费不计算(见 `AppData.pet-ring-*`)。和 `week-card-height` 一个路子。
const RING_ICON: f32 = 44.0; // 圆形按钮直径
const RING_GAP: f32 = 20.0; // 按钮内边缘离宠物方块的距离
const RING_SPREAD: f32 = 45.0; // 从正上方往两边各偏多少度
/// 判断「鼠标还在这一圈里」时额外放宽的像素 —— 手抖不该让菜单闪掉
const RING_SLACK: f32 = 10.0;

/// 从宠物中心到图标中心的距离
fn ring_radius(pet: f32) -> f32 {
    pet / 2.0 + RING_GAP + RING_ICON / 2.0
}

/// 这一圈要给窗口顶上留多高。**和宠物大小无关** —— 最上面那个图标比宠物头顶
/// 高出 `gap + icon` 那么多,宠物多大都一样。
fn ring_band(open: bool) -> f32 {
    if open {
        RING_GAP + RING_ICON
    } else {
        0.0
    }
}

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

// ── AI 对话 ────────────────────────────────────────────────────────────

/// 流式拉取增量的事件轮询间隔。
///
/// 比帧率还快一点:网络来的包本来就是一坨一坨的,50ms 一次既跟得上,
/// 又不会让 UI 每来一个字就重排一次。
const CHAT_TICK: Duration = Duration::from_millis(50);
/// 一个 tick 里最多处理多少个网络包(剩下的下个 tick 继续)。
/// 不设上限的话,遇到「一口气灌几千个包」的服务会把 UI 饿死。
const CHAT_MAX_PACKETS_PER_TICK: usize = 40;
/// 发给模型的历史条数上限(不含 system 提示词)。
/// 超了就从**最老的**开始丢 —— 桌宠聊天不需要记得住几十轮之前的事,
/// 而上下文越长越贵、越慢。
const CHAT_HISTORY_LIMIT: usize = 20;
/// 草稿本预览的刷新间隔。比聊天那个慢一点:这是「边打字边看」,
/// 太快反而会让滚动位置抖动;120ms 人眼已经觉得是即时的了。
const NOTES_TICK: Duration = Duration::from_millis(120);
/// 对话窗和桌宠之间的间距(定位用)
const CHAT_GAP: i32 = 12;

/// 后台线程 → UI 线程的事件
enum ChatEvent {
    /// 一小段正文增量
    Delta(String),
    /// 流正常结束
    Done,
    /// 出错(网络、HTTP 状态、服务端 error 包、被中断)
    Failed(String),
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
    /// 上一次算出来的宽度。**高度没变但宽度变了也得动窗口**(宠物调大时那一圈
    /// 图标会撑宽窗口),所以那个「尺寸没变就不动」的短路要连它一起比。
    pet_w: Cell<f32>,
    /// 悬停时那一圈功能图标开着没有(见 State::tick_pet_ring)
    pet_ring: Cell<bool>,
    /// 刚点过这一圈里的某个图标 —— 收起来,直到鼠标离开那一圈为止。
    /// 「选完就收」是菜单该有的样子;不这么写的话光标还停在图标上,圈会一直开着。
    pet_ring_dismissed: Cell<bool>,
    /// AI 对话窗口
    chat: RefCell<Option<Rc<ChatWindow>>>,
    /// 对话窗那份消息模型(宿主和 UI 共用同一个 VecModel,流式时用 set_row_data 改最后一条)
    chat_model: Rc<VecModel<ChatMsg>>,
    /// 发给模型的对话历史(**不含** system 提示词,那个每次现拼)
    chat_history: RefCell<Vec<ai::Message>>,
    chat_busy: Cell<bool>,
    /// 状态行:忙时是「正在思考…」,闲时非空说明上一条是错误
    chat_status: RefCell<String>,
    /// 本次请求的取消标志。**每次请求新建一个**,不复用全局标志位 ——
    /// 参考项目里就有「上一次忘了复位,下一次一按就被取消」的坑。
    chat_cancel: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    /// 后台线程送增量回来的那一端
    chat_rx: RefCell<Option<std::sync::mpsc::Receiver<ChatEvent>>>,
    /// 拉增量的定时器,只在有请求在跑时开着
    chat_timer: RefCell<Option<Timer>>,
    /// 每追加一次就 +1,UI 收到就把消息列表滚到底
    chat_scroll_tick: Cell<i32>,
    /// 接口设置面板开着没有
    chat_config_open: Cell<bool>,
    /// 接口配置(base_url / api_key / model),单独存在 ai.json
    chat_cfg: RefCell<model::AiConfig>,
    chat_cfg_path: PathBuf,
    /// Markdown 笔记窗口
    notes: RefCell<Option<Rc<NotesWindow>>>,
    /// 笔记库目录(`%APPDATA%\nuntel\notes\`)。文件系统那点事全交给 `notes` 模块,
    /// 这里只存目录本身 —— 别的地方不要再自己拼路径。
    notes_dir: PathBuf,
    /// 当前打开的是哪一篇(空 = 一篇都没选)。文件名既是显示名也是 id。
    notes_current: RefCell<String>,
    /// 库里的文件列表(推给左边那列)
    notes_model: Rc<VecModel<NoteItem>>,
    /// 分屏 / 只编辑 / 只预览
    notes_view: Cell<MdView>,
    /// 左边那列收没收起来
    notes_list_shown: Cell<bool>,
    /// 源码改动还没存盘
    notes_dirty: Cell<bool>,
    notes_status: RefCell<String>,
    /// 预览用的块
    notes_blocks: Rc<VecModel<MdBlock>>,
    /// 上一次切块时的源码,用来判断「要不要重新解析」
    notes_last_source: RefCell<String>,
    /// 解码好的图片。键是 **(绝对路径, 文件修改时间)** —— 带时间是故意的:
    /// 换了图不用重启就能看到新的。见 `State::image_slot`。
    image_cache: RefCell<HashMap<(PathBuf, Option<SystemTime>), ImageSlot>>,
    /// 语法高亮的结果,键是 **(语言, 代码的哈希)**。见 `State::code_lines`。
    /// 有上限:打字时每次改动都会算一份新的,不封顶会一直涨。
    code_cache: RefCell<HashMap<(String, u64), CodeLines>>,
    notes_tick: Cell<i32>,
    /// 预览的刷新定时器(只在窗口可见时跑)
    notes_timer: RefCell<Option<Timer>>,
    /// 单击桌宠的延迟定时器(见 State::pet_clicked)
    pet_click_timer: RefCell<Option<Timer>>,
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

    /// 切亮暗。原先主窗口标题行有个太阳/月亮按钮来回切(`cycle_theme`),
    /// 现在改成设置窗口里「亮色 / 暗色」两个 Chip 直接指定 —— 入口和「外观主题」
    /// 放在一起,两档也就用不着「循环」这个动作了。
    ///
    /// 写的是**主窗口**那份 Theme(它是唯一数据源,`push_theme` 从它读 preference),
    /// 推给其余窗口由 `push_theme` 负责 —— 设置窗口里那两个 Chip 因此会立刻跟着变。
    fn set_theme_preference(&self, value: ThemePreference) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.global::<Theme>().get_preference() == value {
            return; // 点的就是当前这个,别白写一次盘
        }
        ui.global::<Theme>().set_preference(value);
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

        // 对话窗和草稿本也是独立窗口,同样各持一份 Theme —— 漏了它切外观就不跟着变
        let chat = self.chat.borrow().as_ref().cloned();
        if let Some(chat) = chat {
            let theme = chat.global::<Theme>();
            theme.set_preference(preference);
            theme.set_appearance(appearance);
        }
        let notes = self.notes.borrow().as_ref().cloned();
        if let Some(notes) = notes {
            let theme = notes.global::<Theme>();
            theme.set_preference(preference);
            theme.set_appearance(appearance);
        }

        // 桌宠也是独立窗口,而且它的气泡/输入条/悬停那一圈都吃主题色。
        // 这条原来漏了:桌宠那份 Theme 一直停在默认值(`preference: system`),
        // 于是**系统是亮色、应用选了暗色**时,桌宠会自己亮着 —— 新加的那圈圆形
        // 按钮(一大片不透明底色)把这件事放大到一眼就能看见。
        let pet = self.pet.borrow().as_ref().cloned();
        if let Some(pet) = pet {
            let theme = pet.global::<Theme>();
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
        // 两个滑杆要显示当前值 —— 设置窗口有自己那份 AppData,不推就一直是默认值。
        // 开机自启同理,而且它**每次都要重新读注册表**:状态可能在别处被改过
        // (任务管理器「启动」页、别的工具),不能拿启动时读到的那份一直用。
        self.push_pet_settings();
        self.push_autostart();

        // 走 reveal 而不是 show:见 reveal 的说明,直接 show 会只画一部分
        if let Err(err) = self.reveal(&*settings, platform::SETTINGS_TITLE) {
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
        let pet_size = self.pet_size.get();
        app.set_pet_size(pet_size);
        app.set_pet_count(self.remaining_count());
        // ⚠️ **这几样的推送顺序是有讲究的**:`pet-ring-band`(宠物盒子上方给那一圈
        // 留的空白)和 `pet-adding`/`pet-bubble`(叠在盒子上的气泡/输入条)在布局里
        // 是**叠着**的,两边同时为真时布局要的最小高度(= 条 + 间距 + 宠物盒子)
        // 会比窗口还高 —— Slint 会把窗口撑到最小高度,而且**只撑不缩**,
        // 底边就掉下去了(宠物整体低 24px,见 `State::resync_pet_size`)。
        // 所以**先把归零的那一样推下去**:留白要归零时先推留白,条要归零时先推条。
        // 两边都非零时反而安全 —— 那时窗口本来就把两样都算进去了。
        let band = ring_band(self.pet_ring.get());
        let adding = self.pet_adding.get();
        let bubble = self.pet_bubble();
        if band > 0.0 {
            app.set_pet_adding(adding);
            app.set_pet_bubble(bubble.as_str().into());
            app.set_pet_ring_band(band);
        } else {
            app.set_pet_ring_band(0.0);
            app.set_pet_adding(adding);
            app.set_pet_bubble(bubble.as_str().into());
        }
        // 那一圈图标的几何(窗口大小就是按它定的,所以只在宿主这边算)
        app.set_pet_ring_open(self.pet_ring.get());
        app.set_pet_ring_radius(ring_radius(pet_size));
        app.set_pet_ring_icon(RING_ICON);
        app.set_pet_ring_spread(RING_SPREAD);
        self.push_pet_frame();

        // 气泡/输入条/那一圈图标都是靠**窗口高度**让出位置的(布局贴底,多出来的
        // 那截就长在头顶)。以前这里只推文字、不重排,于是**提醒气泡出现时窗口
        // 还是原来那么高** —— 30px 的气泡被顶到窗口外,用户只能看到最下面一小条
        // (桌宠头顶那条横线)。`layout_pet_window` 自己在尺寸没变时提前返回,
        // 放在这里重复调没有代价。
        self.layout_pet_window();
    }

    /// 只推当前这一帧(动画心跳每翻一帧调一次,别的状态不动)
    fn push_pet_frame(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        let (anim, frame) = self.pet_anim.get();
        if let Some(img) = self.pet_frames.frame(anim, frame) {
            pet.global::<AppData>().set_pet_frame(img);
        }
    }

    /// 悬停判定:鼠标在不在宠物身上 / 还在不在那一圈里。
    ///
    /// **为什么轮询,不用 Slint 的 `has-hover`**:
    /// 1. 鼠标从宠物移向图标时会经过两者之间的**空隙**,`has-hover` 在那儿会闪断
    ///    一下,菜单跟着闪 —— 得再想别的办法托底;
    /// 2. 「输入条开着不弹」「点完收起」「鼠标还在不在圈里」这些规则本来就要宿主
    ///    知道指针在哪;
    /// 3. 判定只是一次圆内测试。
    ///
    /// 代价是最多 40ms 的延迟(PET_TICK 的周期)和 25 次/秒的 `GetCursorPos`,
    /// 都感觉不到。
    fn tick_pet_ring(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        if !pet.window().is_visible() {
            return;
        }

        self.resync_pet_size(&pet);
        let (in_pet, in_ring) = self.pet_hover(&pet);

        // 点完图标先收着,等鼠标离开这一圈再允许重新弹
        if !in_ring {
            self.pet_ring_dismissed.set(false);
        }

        // 头顶有气泡/输入条时不弹:正在记事情的时候弹一排图标出来是干扰
        let busy = self.pet_adding.get() || !self.pet_bubble().is_empty();
        let open = self.pet_ring.get();
        // **开**判宠物本体、**保持**判整圈 —— 这层迟滞是必须的,
        // 不然鼠标刚往图标那边走,菜单就没了
        let want =
            !busy && !self.pet_ring_dismissed.get() && if open { in_ring } else { in_pet };
        if want != open {
            self.set_pet_ring(want);
        }
    }

    /// 盯着窗口的实际尺寸,**被别人改过就按我们的算法摆回去**。
    ///
    /// 桌宠的窗口高度只有我们在定,但改它的不止我们:
    /// - `nudge_window` 启动时顶的那 1px(见那儿);
    /// - **Slint 自己** —— 布局的*最小*尺寸比窗口还高时,它会
    ///   `adjust_window_size_to_satisfy_constraints` 把窗口撑到最小尺寸。而最小尺寸
    ///   是「输入条 + 间距 + 宠物盒子」叠出来的,只要界面上有一帧是「输入条开着 +
    ///   `pet-ring-band` 还没归零」,最小尺寸就比窗口高 24px。它**只撑不缩**,
    ///   于是窗口永久停在 278:底边从 1368 掉到 1392,宠物整体低 24px。
    ///
    /// 与其指望每一处推送顺序都不出错(踩过一次了),不如在这里兜底:每 40ms
    /// 对一次,不对就重排。位置是从「窗口现在的位置 + 我们算的高度」反推的,
    /// 所以纠正回来的是**我们想要的**那个底边,不会被量到的错尺寸带偏。
    fn resync_pet_size(&self, pet: &PetWindow) {
        let old_h = self.pet_h.get();
        if old_h <= 0.0 {
            return; // 还没摆过,第一次由 layout_pet_window 负责
        }
        let scale = pet.window().scale_factor() as f32;
        let Some((_, _, lw, lh)) = platform::window_rect(platform::PET_TITLE) else { return };
        let (ew, eh) = ((self.pet_w.get() * scale).round() as i32, (old_h * scale).round() as i32);
        if (lw - ew).abs() <= 1 && (lh - eh).abs() <= 1 {
            return;
        }
        // 顺带把界面那边的三样记下来 —— 撑窗的罪魁就是它们的一个组合
        let app = pet.global::<AppData>();
        platform::log(&format!(
            "桌宠窗口尺寸被外部改过(量到 {lw}x{lh},应该是 {ew}x{eh});界面:ring={} band={} adding={} bubble={:?}",
            app.get_pet_ring_open(),
            app.get_pet_ring_band(),
            app.get_pet_adding(),
            app.get_pet_bubble().as_str(),
        ));
        // **不能靠把 pet_h 清零来绕过「尺寸没变就早退」**:那个缓存是反推底边用的
        // 「我们应该有多高」,清零之后整窗会按 0 高度算位置 —— 窗口往上跳一整层
        // (实测跳了 190px),然后又被下面的检查逮到,越修越远。
        self.layout_pet_window_forced();
    }

    /// 指针在不在宠物身上 / 还在不在那一圈里 —— `(in_pet, in_ring)`。
    ///
    /// 从 `tick_pet_ring` 里拎出来的:收起输入条时要用同一个判定决定要不要立刻
    /// 把那一圈叫回来(见 `toggle_pet_input`)。
    ///
    /// 全用**物理**像素比:指针是系统的物理坐标,窗口位置/尺寸也是物理的,
    /// 而 pet-size 是逻辑的(高 DPI 上要乘缩放,不然判定区会小一圈)。
    fn pet_hover(&self, pet: &PetWindow) -> (bool, bool) {
        let scale = pet.window().scale_factor() as f32;
        let pet_px = self.pet_size.get() * scale;
        let pos = pet.window().position();
        let size = pet.window().size();
        // 宠物在窗口里横向居中、贴着底边(见 ui/pet.slint 的 pet-box):
        // 所以不管那一圈开着没有(窗口会变高),圆心都能这么算
        let (cx, cy) = (
            pos.x as f32 + size.width as f32 / 2.0,
            pos.y as f32 + size.height as f32 - pet_px / 2.0,
        );
        let (cursor_x, cursor_y) = platform::cursor_pos();
        let (dx, dy) = (cursor_x as f32 - cx, cursor_y as f32 - cy);

        // 整圈的范围:图标摆在这个半径上,再往外放一个图标半径 + 余量
        let hit =
            ring_radius(self.pet_size.get()) * scale + (RING_ICON / 2.0 + RING_SLACK) * scale;
        let in_ring = dx * dx + dy * dy <= hit * hit;
        let in_pet = dx.abs() <= pet_px / 2.0 + 2.0 * scale && dy.abs() <= pet_px / 2.0;
        (in_pet, in_ring)
    }

    /// 开/关那一圈图标:改状态、重排窗口(高度要跟着变)、推给 UI。
    fn set_pet_ring(&self, open: bool) {
        self.pet_ring.set(open);
        self.layout_pet_window();
        if let Some(pet) = self.pet.borrow().clone() {
            let app = pet.global::<AppData>();
            app.set_pet_ring_open(open);
            app.set_pet_ring_band(ring_band(open));
        }
    }

    /// 点了那一圈里的某个图标。开对应的窗口,然后把这圈收起来。
    fn pet_ring_action(&self, action: &str) {
        self.pet_ring_dismissed.set(true);
        self.set_pet_ring(false);
        match action {
            "notes" => self.open_notes(),
            "list" => self.open_task_list(),
            "settings" => self.open_settings(),
            // 图标是写死在 ui/pet.slint 里的,走到这儿说明两边对不上号了
            other => platform::log(&format!("桌宠那一圈里有个不认识的图标: {other}")),
        }
    }

    /// 动画心跳:累积时间,够一帧就翻。
    fn tick_pet(&self) {
        let Some(pet) = self.pet.borrow().clone() else { return };
        // 看不见就不烧 CPU(收进托盘、被别的窗口盖住时没必要动)
        if !pet.window().is_visible() {
            return;
        }
        // 顺带看一眼鼠标(放在动画逻辑前面:那段有可能提前 return)
        self.tick_pet_ring();

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

    // ── Markdown 草稿本 ────────────────────────────────────────────────

    /// 把源码重新切块并推给预览。
    ///
    /// 只在**源码真的变了**的时候调用(定时器里比对),不然每次轮询都重建一遍模型,
    /// 预览会一直闪、滚动位置也保不住。
    fn push_notes_blocks(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        let source = notes.global::<AppData>().get_md_source().to_string();
        if *self.notes_last_source.borrow() == source {
            return;
        }
        *self.notes_last_source.borrow_mut() = source.clone();

        let base = self.note_base_dir(&self.notes_current.borrow());
        let blocks: Vec<MdBlock> = markdown::parse(&source)
            .into_iter()
            .map(|b| self.to_ui_block(b, base.as_deref()))
            .collect();
        self.notes_blocks.set_vec(blocks);

        let app = notes.global::<AppData>();
        app.set_md_blocks(ModelRc::from(self.notes_blocks.clone()));
        self.notes_tick.set(self.notes_tick.get() + 1);
        app.set_md_tick(self.notes_tick.get());
        // 打字就算「没存」——状态行会变色提醒
        if !self.notes_dirty.get() && !source.is_empty() {
            self.set_notes_dirty(true);
        }
    }

    fn set_notes_dirty(&self, dirty: bool) {
        self.notes_dirty.set(dirty);
        if let Some(notes) = self.notes.borrow().as_ref() {
            notes.global::<AppData>().set_md_dirty(dirty);
        }
    }

    fn set_notes_status(&self, text: &str) {
        *self.notes_status.borrow_mut() = text.to_string();
        if let Some(notes) = self.notes.borrow().as_ref() {
            notes.global::<AppData>().set_md_status(text.into());
        }
    }

    /// 存盘。关窗口、切笔记之前都会走一次(不然写了一半的笔记就没了)。
    ///
    /// 存的是**当前这一篇**;一篇都没选(库是空的)就什么都不做。
    fn save_notes(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        let name = self.notes_current.borrow().clone();
        if name.is_empty() {
            return;
        }
        let source = notes.global::<AppData>().get_md_source().to_string();
        let shown = self.note_path_text(&name);
        if !self.notes_dirty.get() {
            self.set_notes_status(&format!("已保存到 {shown}"));
            return;
        }
        match notes::write(&self.notes_dir, &name, &source) {
            Ok(()) => {
                self.set_notes_dirty(false);
                self.set_notes_status(&format!("已保存到 {shown}"));
            }
            Err(err) => {
                // 存不上要说清楚:用户以为存了、其实没有,那才是真的丢东西
                platform::log(&format!("保存笔记失败: {err}"));
                self.set_notes_status(&format!("保存失败: {err}"));
            }
        }
    }

    /// 某一篇的完整路径,给状态栏显示用。名字不合法时退回整个库目录。
    fn note_path_text(&self, name: &str) -> String {
        notes::path_in(&self.notes_dir, name)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.notes_dir.display().to_string())
    }

    /// 某一篇笔记**所在的目录** —— 正文里 `![](相对路径)` 的基准。
    ///
    /// ⚠️ 参数是**篇名**,不是「当前篇」:`load_note` 是先把内容切块、后面才更新
    /// `notes_current` 的,从 self 上读会拿到上一篇的目录。
    fn note_base_dir(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        notes::path_in(&self.notes_dir, name).and_then(|p| p.parent().map(Path::to_path_buf))
    }

    /// 预览里点了任务勾选框:把源码那一行的 `[ ]`/`[x]` 翻过来。
    ///
    /// **改的是源码**(推回 `md-source`),不是只改显示 —— 编辑器里、存到盘上都跟着变。
    /// 下一拍 `push_notes_blocks` 发现源码变了会重新切块,勾选框自己就更新了。
    fn toggle_note_task(&self, line: i32) {
        if line < 0 {
            return;
        }
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        let app = notes.global::<AppData>();
        let source = app.get_md_source().to_string();
        // 行号越界、或者那行已经不是任务了(用户刚在编辑器里改过)—— 什么都不做,
        // 下一拍重新切块时界面会对齐
        let Some(flipped) = markdown::toggle_task_line(&source, line as usize) else { return };
        app.set_md_source(flipped.as_str().into());
        self.set_notes_dirty(true);
    }

    /// 把一篇笔记的内容灌进界面(不负责保存上一篇 —— 调用方先 `save_notes`)。
    fn load_note(&self, name: &str) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        let text = notes::read(&self.notes_dir, name);
        *self.notes_last_source.borrow_mut() = text.clone();
        // ⚠️ 基准目录要用**参数里的 name**:下面几行才把 notes_current 改成这一篇,
        // 从 self 上读会拿到上一篇的目录
        let base = self.note_base_dir(name);
        let blocks: Vec<MdBlock> = markdown::parse(&text)
            .into_iter()
            .map(|b| self.to_ui_block(b, base.as_deref()))
            .collect();
        self.notes_blocks.set_vec(blocks);

        *self.notes_current.borrow_mut() = name.to_string();
        {
            let app = notes.global::<AppData>();
            app.set_md_source(text.as_str().into());
            app.set_md_blocks(ModelRc::from(self.notes_blocks.clone()));
            app.set_md_current(name.into());
            app.set_md_dirty(false);
        }
        self.notes_dirty.set(false);
        self.set_notes_status(&self.note_path_text(name));
    }

    /// 一篇都没选时的界面(库是空的,或者刚把当前这篇删掉)。
    fn show_empty_note(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        *self.notes_current.borrow_mut() = String::new();
        // 置成空串,免得下面这个定时器把空源码当成「改过了」又去重建一遍
        *self.notes_last_source.borrow_mut() = String::new();
        self.notes_blocks.set_vec(Vec::new());
        {
            let app = notes.global::<AppData>();
            app.set_md_source("".into());
            app.set_md_blocks(ModelRc::from(self.notes_blocks.clone()));
            app.set_md_current("".into());
            app.set_md_dirty(false);
        }
        self.notes_dirty.set(false);
        self.set_notes_status(&format!("笔记库: {}", self.notes_dir.display()));
    }

    /// 把库里的文件列表推给左边那一列
    fn push_notes_list(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        let names = notes::list(&self.notes_dir);
        self.notes_model
            .set_vec(names.iter().map(|n| NoteItem { name: n.as_str().into() }).collect::<Vec<_>>());
        let app = notes.global::<AppData>();
        app.set_md_notes(ModelRc::from(self.notes_model.clone()));
        app.set_md_current(self.notes_current.borrow().as_str().into());
    }

    /// 把「分屏/单栏」和「列表收没收」推给界面
    fn push_notes_layout(&self) {
        if let Some(notes) = self.notes.borrow().as_ref() {
            let app = notes.global::<AppData>();
            app.set_md_view(self.notes_view.get());
            app.set_md_list_shown(self.notes_list_shown.get());
        }
    }

    /// 换一篇:**先把手上这篇存了再切**,不然改了一半的内容会被下一篇顶掉。
    fn select_note(&self, name: &str) {
        if name.is_empty() || *self.notes_current.borrow() == name {
            return;
        }
        self.save_notes();
        self.load_note(name);
        // 列表里那一行的高亮是拿 md-current 比的,load_note 里已经推过了
    }

    /// 新建一篇。
    fn new_note(&self) {
        self.save_notes();
        match notes::create(&self.notes_dir) {
            Ok(name) => {
                self.push_notes_list();
                self.load_note(&name);
            }
            Err(err) => self.set_notes_status(&err),
        }
    }

    /// 改名。名字的净化和撞名判断都在 `notes` 模块里做,失败就把话说在状态栏上。
    fn rename_note(&self, old: &str, new: &str) {
        match notes::rename(&self.notes_dir, old, new) {
            Ok(clean) => {
                // 改的是当前这篇:内容没动,只要跟着换个名字
                if *self.notes_current.borrow() == old {
                    *self.notes_current.borrow_mut() = clean.clone();
                }
                self.push_notes_list();
                self.set_notes_status(&format!("已改名为「{clean}」"));
            }
            Err(err) => self.set_notes_status(&err),
        }
    }

    /// 删掉一篇(**直接删文件**)。界面上那个按钮点了两次才会走到这儿。
    fn delete_note(&self, name: &str) {
        if let Err(err) = notes::remove(&self.notes_dir, name) {
            self.set_notes_status(&err);
            return;
        }
        platform::log(&format!("删掉笔记「{name}」"));

        if *self.notes_current.borrow() == name {
            // 删的就是当前这篇:先把当前清掉再挑下一篇,免得 `save_notes`
            // 半路把刚删的内容又写回一个新文件
            self.show_empty_note();
            self.push_notes_list();
            if let Some(next) = notes::most_recent(&self.notes_dir) {
                self.load_note(&next);
            }
        } else {
            self.push_notes_list();
        }
        self.set_notes_status(&format!("已删除「{name}」"));
    }

    /// 切 分屏 / 只编辑 / 只预览。
    ///
    /// 顺带把列表也收起来 —— 单栏要的就是「铺满整个窗口」。
    /// (列表开关还能把它叫回来,不然只编辑的时候没法换笔记。)
    fn set_notes_view(&self, view: MdView) {
        self.notes_view.set(view);
        self.notes_list_shown.set(view == MdView::Split);
        self.push_notes_layout();
    }

    /// 收起 / 展开左边那列
    fn toggle_notes_list(&self) {
        let shown = !self.notes_list_shown.get();
        self.notes_list_shown.set(shown);
        self.push_notes_layout();
    }

    /// 打开笔记窗口(第一次进来就把库列出来、挑一篇打开)
    fn open_notes(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else {
            platform::log("笔记窗口没建起来");
            return;
        };
        // 老的单文件笔记搬进库。搬完老文件就没了,所以这个调用天然只生效一次。
        notes::migrate_legacy(&self.notes_dir);

        let current = self.notes_current.borrow().clone();
        if !current.is_empty() {
            // 选过(窗口关掉又打开):重新从盘上读一次,别拿内存里的旧副本
            self.load_note(&current);
        } else if let Some(name) = notes::most_recent(&self.notes_dir) {
            self.load_note(&name);
        } else {
            // 库是空的。**不自动新建** —— 用户可能刚把笔记全删了,
            // 一进来又冒出一篇会很烦。空状态里有「+」,点一下就有。
            self.show_empty_note();
        }
        self.push_notes_list();
        self.push_notes_layout();

        self.layout_notes_window();
        if let Err(err) = self.reveal(&*notes, platform::NOTES_TITLE) {
            platform::log(&format!("显示笔记窗口失败: {err}"));
        }
        platform::bring_to_front(platform::NOTES_TITLE);

        // 预览刷新定时器:只在窗口开着的时候跑
        let Some(me) = self.me.borrow().upgrade() else { return };
        if self.notes_timer.borrow().is_none() {
            *self.notes_timer.borrow_mut() = Some(Timer::default());
        }
        if let Some(t) = self.notes_timer.borrow().as_ref() {
            t.start(TimerMode::Repeated, NOTES_TICK, move || me.poll_notes());
        }
    }

    /// 定时器里跑:源码变了就重新切块(实时预览就是靠这个)
    fn poll_notes(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        if !notes.window().is_visible() {
            return;
        }
        self.push_notes_blocks();
    }

    /// 关草稿本:**先存再藏**,别让用户白写
    fn close_notes(&self) {
        self.save_notes();
        if let Some(notes) = self.notes.borrow().as_ref() {
            let _ = notes.hide();
        }
        if let Some(t) = self.notes_timer.borrow().as_ref() {
            t.stop();
        }
    }

    /// 把一段 Markdown 追加到笔记末尾(AI 回复旁边的「存到笔记」)。
    ///
    /// **落到哪一篇**:窗口开着就写它正在显示的那篇;没开过就挑最近改过的那篇;
    /// 库是空的就现建一篇。落点必须可预期 —— 每按一次就冒出一篇新笔记是最糟的。
    fn append_to_notes(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        let target = match self.notes_current.borrow().as_str() {
            "" => match notes::most_recent(&self.notes_dir) {
                Some(name) => name,
                None => match notes::create(&self.notes_dir) {
                    Ok(name) => name,
                    Err(err) => {
                        platform::log(&format!("追加到笔记失败: {err}"));
                        return;
                    }
                },
            },
            name => name.to_string(),
        };

        // 窗口正开着这一篇的话,**以界面上的为准**(可能还有没存盘的改动),
        // 别去盘上读一份旧的回来把它顶掉。
        //
        // 句柄先 clone 出来再读属性,别抱着 `RefCell` 的借用去调 Slint ——
        // 读属性会跑绑定,万一哪条绑定绕回来又要借 notes,就直接 panic 了
        // (push_theme 里踩过同一个坑)。
        let open_here = {
            let current = self.notes_current.borrow().clone();
            let window = self.notes.borrow().as_ref().cloned();
            match window {
                Some(notes) if current == target => {
                    Some(notes.global::<AppData>().get_md_source().to_string())
                }
                _ => None,
            }
        };

        let mut body = open_here.unwrap_or_else(|| notes::read(&self.notes_dir, &target));
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("\n---\n\n");
        body.push_str(text);
        body.push('\n');

        if let Err(err) = notes::write(&self.notes_dir, &target, &body) {
            platform::log(&format!("追加到笔记失败: {err}"));
            return;
        }

        // 开着的那篇同步刷新(不然它显示的还是旧内容)
        if let Some(notes) = self.notes.borrow().as_ref().cloned()
            && *self.notes_current.borrow() == target
        {
            let app = notes.global::<AppData>();
            app.set_md_source(body.as_str().into());
            let base = self.note_base_dir(&target);
            let blocks: Vec<MdBlock> = markdown::parse(&body)
                .into_iter()
                .map(|b| self.to_ui_block(b, base.as_deref()))
                .collect();
            self.notes_blocks.set_vec(blocks);
            app.set_md_blocks(ModelRc::from(self.notes_blocks.clone()));
            *self.notes_last_source.borrow_mut() = body.clone();
            self.notes_dirty.set(false);
        }
        // 新建过的话列表得跟着更新
        self.push_notes_list();
        self.set_notes_status(&format!("已追加到「{target}」"));
    }

    /// 摆在主窗口旁边(居中于屏幕,和主窗口错开一点)
    fn layout_notes_window(&self) {
        let Some(notes) = self.notes.borrow().as_ref().cloned() else { return };
        let size = notes.window().size();
        let (wx, wy, ww, wh) = platform::work_area();
        let x = wx + (ww - size.width as i32) / 2;
        let y = wy + (wh - size.height as i32) / 2;
        notes.window().set_position(slint::PhysicalPosition::new(x, y));
    }

    // ── AI 对话 ────────────────────────────────────────────────────────
    //
    // 线程模型:**网络在后台线程跑,UI 只在主线程碰**。
    // 本项目没有 async runtime(全靠 Slint 的 Timer + channel 驱动),所以不学
    // 参考项目引 tokio,而是最朴素的一套:
    //
    //   主线程 start_chat ──spawn──▶ 后台线程(ureq 阻塞读 SSE)──mpsc──▶ 主线程 Timer 轮询
    //
    // 这和托盘图标那套(全局 channel + 定时器轮询)是同一个形状,项目里已经有先例。

    /// 把消息模型 + 运行状态推给对话窗。
    ///
    /// **不推接口配置** —— 那三个字段是双向绑定的,推一次就会把用户正在编辑的
    /// 输入框覆盖掉。配置只在「打开窗口」和「保存之后」推(见 push_chat_config)。
    fn push_chat(&self) {
        let Some(chat) = self.chat.borrow().as_ref().cloned() else { return };
        let app = chat.global::<AppData>();
        app.set_chat_messages(ModelRc::from(self.chat_model.clone()));
        app.set_chat_busy(self.chat_busy.get());
        app.set_chat_status(self.chat_status.borrow().as_str().into());
        app.set_chat_scroll_tick(self.chat_scroll_tick.get());
        app.set_chat_config_open(self.chat_config_open.get());
    }

    /// 只在「该显示配置」的时候推那三个字段
    fn push_chat_config(&self) {
        let Some(chat) = self.chat.borrow().as_ref().cloned() else { return };
        let cfg = self.chat_cfg.borrow();
        let app = chat.global::<AppData>();
        app.set_chat_base_url(cfg.base_url.as_str().into());
        app.set_chat_api_key(cfg.api_key.as_str().into());
        app.set_chat_model(cfg.model.as_str().into());
    }

    fn set_chat_status(&self, text: &str) {
        *self.chat_status.borrow_mut() = text.to_string();
        if let Some(chat) = self.chat.borrow().as_ref() {
            chat.global::<AppData>().set_chat_status(text.into());
        }
    }

    /// 往对话里追加一条消息(同时推给 UI)
    fn push_chat_msg(&self, msg: ChatMsg) {
        self.chat_model.push(msg);
        self.chat_scroll_tick.set(self.chat_scroll_tick.get() + 1);
        if let Some(chat) = self.chat.borrow().as_ref() {
            chat.global::<AppData>()
                .set_chat_scroll_tick(self.chat_scroll_tick.get());
        }
    }

    /// 设置面板开关
    fn toggle_chat_config(&self) {
        let open = !self.chat_config_open.get();
        self.chat_config_open.set(open);
        if open {
            // 打开时才把当前配置灌进输入框(平时不推,免得覆盖用户的编辑)
            self.push_chat_config();
        }
        if let Some(chat) = self.chat.borrow().as_ref() {
            chat.global::<AppData>().set_chat_config_open(open);
        }
    }

    /// 保存接口配置
    fn save_chat_config(&self, base_url: &str, api_key: &str, model: &str) {
        {
            let mut cfg = self.chat_cfg.borrow_mut();
            cfg.base_url = base_url.trim().to_string();
            cfg.api_key = api_key.trim().to_string();
            cfg.model = model.trim().to_string();
        }
        let result = {
            let cfg = self.chat_cfg.borrow();
            model::save_ai(&self.chat_cfg_path, &cfg)
        };
        match result {
            Ok(()) => {
                self.set_chat_status("");
                self.chat_config_open.set(false);
                if let Some(chat) = self.chat.borrow().as_ref() {
                    chat.global::<AppData>().set_chat_config_open(false);
                }
                self.push_chat_config(); // 回填去掉首尾空格后的值
            }
            Err(err) => self.set_chat_status(&format!("配置没存上: {err}")),
        }
    }

    /// 双击桌宠:弹出对话窗(已经在开着就只叫到前面)
    fn open_chat(&self) {
        let Some(chat) = self.chat.borrow().as_ref().cloned() else {
            platform::log("对话窗口没建起来,双击桌宠没反应");
            return;
        };
        // 第一次打开时如果还没配好接口,直接把设置面板摊开
        if !self.chat_cfg.borrow().is_ready() && self.chat_model.row_count() == 0 {
            self.chat_config_open.set(true);
            self.push_chat_config();
        }
        self.layout_chat_window();
        if let Err(err) = self.reveal(&*chat, platform::CHAT_TITLE) {
            platform::log(&format!("显示对话窗口失败: {err}"));
        }
        self.push_chat();
        platform::bring_to_front(platform::CHAT_TITLE);
    }

    /// 关掉对话窗(只是藏起来,聊天记录留着,下次双击接着看)
    fn close_chat(&self) {
        if let Some(chat) = self.chat.borrow().as_ref() {
            let _ = chat.hide();
        }
        self.cancel_chat();
    }

    /// 把对话窗摆在桌宠**旁边**:右边放得下就放右边,放不下翻到左边。
    ///
    /// 竖直方向让底边和宠物对齐(看起来像从宠物「长」出来的),再整体夹进工作区,
    /// 免得贴边时露出屏幕外。
    fn layout_chat_window(&self) {
        let Some(chat) = self.chat.borrow().as_ref().cloned() else { return };
        let Some(pet) = self.pet.borrow().as_ref().cloned() else { return };

        let pet_pos = pet.window().position();
        let pet_size = pet.window().size();
        let chat_size = chat.window().size();
        let (pw, ph) = (pet_size.width as i32, pet_size.height as i32);
        let (cw, ch) = (chat_size.width as i32, chat_size.height as i32);
        let (wx, wy, ww, wh) = platform::work_area();

        let right = pet_pos.x + pw + CHAT_GAP;
        let left = pet_pos.x - cw - CHAT_GAP;
        // 右边放得下就用右边;否则用左边;两边都放不下(屏幕太窄)就贴着工作区右边缘
        let x = if right + cw <= wx + ww {
            right
        } else if left >= wx {
            left
        } else {
            (wx + ww - cw).max(wx)
        };
        // 底边对齐宠物,再夹进工作区
        let y = (pet_pos.y + ph - ch).clamp(wy, (wy + wh - ch).max(wy));

        chat.window().set_position(slint::PhysicalPosition::new(x, y));
    }

    /// 发一条消息:起后台线程去请求,增量通过 channel 回来
    fn start_chat(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.chat_busy.get() {
            return;
        }

        let cfg = (*self.chat_cfg.borrow()).clone();
        if let Some(missing) = cfg.missing() {
            self.chat_config_open.set(true);
            self.push_chat_config();
            if let Some(chat) = self.chat.borrow().as_ref() {
                chat.global::<AppData>().set_chat_config_open(true);
            }
            self.set_chat_status(&format!("还差「{missing}」—— 在上面填好再发。"));
            return;
        }

        // 1. 用户那条进模型和历史
        self.push_chat_msg(ChatMsg {
            who: ChatWho::User,
            text: text.into(),
            streaming: false,
            // 用户消息不按 Markdown 解析:随手打的表情、星号不该被吃掉
            blocks: self.chat_blocks(""),
        });
        self.chat_history.borrow_mut().push(ai::Message {
            role: ai::Role::User,
            content: text.to_string(),
        });
        // 2. 助手那条先占位(流式往里追加);历史里也先放一条空的,
        //    这样每个增量只需要 push_str,不用每次去找「最后一条是不是助手」
        self.push_chat_msg(ChatMsg {
            who: ChatWho::Assistant,
            text: "".into(),
            streaming: true,
            blocks: self.chat_blocks(""),
        });
        self.chat_history.borrow_mut().push(ai::Message {
            role: ai::Role::Assistant,
            content: String::new(),
        });

        // 3. 请求体:system 提示词 + 最近若干轮
        let pending = self.remaining_count() as usize;
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut messages = vec![ai::Message {
            role: ai::Role::System,
            content: ai::system_prompt(pending, &today),
        }];
        {
            let history = self.chat_history.borrow();
            let start = history.len().saturating_sub(CHAT_HISTORY_LIMIT);
            messages.extend_from_slice(&history[start..]);
        }

        // 4. 清掉输入框、切忙碌态、桌宠切「看书」
        self.chat_busy.set(true);
        if let Some(chat) = self.chat.borrow().as_ref() {
            let app = chat.global::<AppData>();
            app.set_chat_draft("".into());
            app.set_chat_busy(true);
        }
        self.set_chat_status("正在思考…");
        self.play_pet_cue("read");

        // 5. 开工
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        *self.chat_cancel.borrow_mut() = Some(cancel.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        *self.chat_rx.borrow_mut() = Some(rx);
        let spawned = std::thread::Builder::new()
            .name("rgui-ai-chat".into())
            .spawn(move || run_chat(cfg, messages, cancel, tx));
        if let Err(err) = spawned {
            self.end_chat();
            self.set_chat_status(&format!("起不了后台线程: {err}"));
            return;
        }

        // 6. 开轮询定时器
        let Some(me) = self.me.borrow().upgrade() else { return };
        if self.chat_timer.borrow().is_none() {
            *self.chat_timer.borrow_mut() = Some(Timer::default());
        }
        if let Some(t) = self.chat_timer.borrow().as_ref() {
            t.start(TimerMode::Repeated, CHAT_TICK, move || me.poll_chat());
        }
    }

    /// 拉一次增量(定时器里跑,主线程)
    fn poll_chat(&self) {
        let mut finished = false;
        let mut buffer = String::new();
        {
            let rx = self.chat_rx.borrow();
            let Some(rx) = rx.as_ref() else { return };
            for _ in 0..CHAT_MAX_PACKETS_PER_TICK {
                match rx.try_recv() {
                    // 同一 tick 里的增量**攒成一整块再写模型**:一次网络读可能带来
                    // 好几个包,逐个改模型会让 UI 白重排好几次
                    Ok(ChatEvent::Delta(t)) => buffer.push_str(&t),
                    Ok(ChatEvent::Done) => {
                        finished = true;
                        break;
                    }
                    Ok(ChatEvent::Failed(msg)) => {
                        finished = true;
                        self.fail_chat(&msg, buffer.is_empty());
                        buffer.clear();
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // 线程没了却没发 Done:算正常收尾,别把界面卡在「忙碌」上
                        finished = true;
                        break;
                    }
                }
            }
        }
        if !buffer.is_empty() {
            self.append_chat_delta(&buffer);
        }
        if finished {
            self.end_chat();
        }
    }

    /// 把一段增量追加到最后一条助手消息上
    fn append_chat_delta(&self, text: &str) {
        let idx = self.chat_model.row_count().saturating_sub(1);
        let Some(ChatMsg { who, text: cur, streaming, blocks: _ }) = self.chat_model.row_data(idx)
        else {
            return;
        };
        if who != ChatWho::Assistant {
            return;
        }
        let full = format!("{cur}{text}");
        self.chat_model.set_row_data(
            idx,
            ChatMsg {
                who,
                // 每个 tick 把整段重新切一次块。增量是按 tick 攒过的(见 poll_chat),
                // 所以这里是 ~20 次/秒、每次几 KB 的量级,完全无所谓;
                // 换来的好处是不用维护「哪些块已经定型了」这种增量状态。
                blocks: self.chat_blocks(&full),
                text: full.as_str().into(),
                streaming,
            },
        );
        if let Some(last) = self.chat_history.borrow_mut().last_mut() {
            last.content.push_str(text);
        }
        self.chat_scroll_tick.set(self.chat_scroll_tick.get() + 1);
        if let Some(chat) = self.chat.borrow().as_ref() {
            chat.global::<AppData>()
                .set_chat_scroll_tick(self.chat_scroll_tick.get());
        }
    }

    /// 出错:把错误当成一条系统提示写进对话(跟着记录一起滚动),
    /// 状态行也留一句,免得用户以为是自己没发出去。
    fn fail_chat(&self, msg: &str, placeholder_empty: bool) {
        let idx = self.chat_model.row_count().saturating_sub(1);
        if placeholder_empty {
            // 助手那条还空着,直接把它换成系统提示,不留空气泡
            if let Some(row) = self.chat_model.row_data(idx) {
                if row.who == ChatWho::Assistant && row.text.is_empty() {
                    self.chat_model.set_row_data(
                        idx,
                        ChatMsg {
                            who: ChatWho::Note,
                            text: msg.into(),
                            streaming: false,
                            blocks: self.chat_blocks(""),
                        },
                    );
                    // 历史里那条空助手消息也去掉,免得下一轮请求带着一条空的
                    let mut history = self.chat_history.borrow_mut();
                    if history
                        .last()
                        .is_some_and(|m| m.role == ai::Role::Assistant && m.content.is_empty())
                    {
                        history.pop();
                    }
                    self.set_chat_status(msg);
                    self.chat_scroll_tick.set(self.chat_scroll_tick.get() + 1);
                    return;
                }
            }
        }
        // 已经有正文了(流到一半断的):正文留着,后面补一条提示
        self.push_chat_msg(ChatMsg {
            who: ChatWho::Note,
            text: msg.into(),
            streaming: false,
            blocks: self.chat_blocks(""),
        });
        self.set_chat_status(msg);
    }

    /// 收尾:停定时器、清忙碌、桌宠回待机
    fn end_chat(&self) {
        self.chat_rx.borrow_mut().take();
        *self.chat_cancel.borrow_mut() = None;
        // ⚠️ 别写 `if let Some(t) = self.chat_timer.borrow_mut().take() { ... borrow_mut() ... }`:
        // if-let 的临时借用活到**整个 if let 表达式结束**(含块体),块里再借一次
        // 就是 RefCell already borrowed 直接 panic(这个坑刚踩过,程序当场没了)。
        // 只借一次、原地停就行 —— Timer 留在槽里,下次 start 直接复用。
        if let Some(t) = self.chat_timer.borrow().as_ref() {
            t.stop();
        }
        // 最后一条不再闪光标
        let idx = self.chat_model.row_count().saturating_sub(1);
        if let Some(row) = self.chat_model.row_data(idx) {
            if row.streaming {
                self.chat_model.set_row_data(
                    idx,
                    ChatMsg {
                        who: row.who,
                        // 最后再切一次:收尾的这一轮增量可能还没进过切块
                        blocks: self.chat_blocks(&row.text),
                        text: row.text,
                        streaming: false,
                    },
                );
            }
        }
        self.chat_busy.set(false);
        if let Some(chat) = self.chat.borrow().as_ref() {
            chat.global::<AppData>().set_chat_busy(false);
        }
        // 清掉「正在思考…」。不清的话它会在不忙之后落进「错误提示」那个分支,
        // 变成一行红字挂在那儿(实测就是这样)
        self.set_chat_status("");
        // 桌宠从「看书」回待机轮播
        self.back_to_idle();
    }

    /// 中断这次生成(点停止、关窗口时都会走这儿)
    fn cancel_chat(&self) {
        if let Some(flag) = self.chat_cancel.borrow().as_ref() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if self.chat_busy.get() {
            self.end_chat();
            let idx = self.chat_model.row_count().saturating_sub(1);
            let empty_assistant = self
                .chat_model
                .row_data(idx)
                .is_some_and(|r| r.who == ChatWho::Assistant && r.text.is_empty());
            if empty_assistant {
                self.chat_model.set_row_data(
                    idx,
                    ChatMsg {
                        who: ChatWho::Note,
                        text: "已中断".into(),
                        streaming: false,
                        blocks: self.chat_blocks(""),
                    },
                );
            }
        }
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
    ///
    /// 位置一律从**宠物中心**反推,不是拿窗口当前位置加减差值。宠物在窗口里
    /// 横向居中、贴着底边,所以「中心」是窗口怎么长都不动的那个点;而窗口的左上角
    /// 每次都在变。从中心反推还有一个好处:它只依赖**当前**的矩形,和上次算的
    /// 尺寸无关 —— 缓存和实际一旦错开(窗口还没建出来、被系统挪过、刚改完尺寸
    /// 还没收到事件),加减差值那套就会把宠物一步步带偏,而且回不来。
    ///
    /// 落地只能用**一次** `SetWindowPos`,原因见 `platform::set_window_rect`。
    fn layout_pet_window(&self) {
        self.layout_pet_window_inner(false);
    }

    /// 同上,但**跳过「尺寸没变就别动」那条短路**(`resync_pet_size` 用)。
    fn layout_pet_window_forced(&self) {
        self.layout_pet_window_inner(true);
    }

    fn layout_pet_window_inner(&self, force: bool) {
        let Some(pet) = self.pet.borrow().clone() else { return };

        // 高度 = 宠物 + **顶上那一块** + 角标余量。**尺寸恒定,不随内容伸缩。**
        //
        // 顶上那一块是三样东西抢的同一块地方:提醒气泡(36)、快速输入条(42)、
        // 那一圈图标(`RING_GAP + RING_ICON`),按最高的那个留(现在恒为 64)。
        //
        // ⚠️ **为什么不能按内容伸缩**(为此踩了整整一轮,详见 §32):
        // 改窗口尺寸是「`SetWindowPos` 落地」和「Slint 重画」两件事,而 Slint 的重画
        // 按帧节流 —— 中间那一拍里 DWM 拿的是**旧画面**,铺在新的矩形上,尺寸差多少
        // 宠物就在那一帧里偏多少。悬停进出宠物、点开输入条、来一条提醒……每次都会
        // 闪一下(60Hz 下抓屏抓到过一到两帧)。尺寸钉死之后,**宠物在屏幕上就是一块
        // 完全不动的图**,代价是头顶常驻一块透明的空当(往上约 106px)。
        //
        // ⚠️ 那块空当现在**会吃掉落在上面的点击** —— 要治本得再给窗口设区域
        // (`SetWindowRgn`)把透明的部分挖掉,还没做,见 §32.6。
        //
        // 那 42 的余量也不是随手给的:Slint 会拿「布局的最小高度」跟窗口比,小了就
        // 自己撑大(`adjust_window_size_to_satisfy_constraints`,**只撑不缩**),而布局
        // 算出来的最小高度比宠物盒子还多 42(具体多算了哪一样没查清,反正是恒定的)。
        // 少给这 24 窗口就会被撑大、再被 `resync_pet_size` 拉回来,变成周期性闪烁。
        // 见 §32.4 坑 2。
        let pet_size = self.pet_size.get();
        let h = pet_size + 42.0 + ring_band(true);
        // 宽度:除了给气泡/输入条留位置,还得容得下 ±spread 那两个图标
        // (宠物调到最大时 240 就不够了,会往外撑)
        let r = ring_radius(pet_size);
        let reach = r * RING_SPREAD.to_radians().sin() + RING_ICON / 2.0 + 6.0;
        let w = PET_WIDTH.max(pet_size + 24.0).max(reach * 2.0);

        // 尺寸没变就别动窗口:否则每次同步都重摆一遍,
        // 用户正拖着的时候会被拽回原处。**宽高都要比** —— 只比高度的话,
        // 「宠物调大 → 环把窗口撑宽」这条路径会被这条短路吞掉。
        let old_h = self.pet_h.get();
        let old_w = self.pet_w.get();
        if !force && old_h > 0.0 && (h - old_h).abs() < 0.5 && (w - old_w).abs() < 0.5 {
            return;
        }

        // 整个几何都用**物理**像素算:窗口矩形、工作区、指针都是物理的,
        // 而 pet-size / 这几个常量是逻辑的(高 DPI 上必须乘缩放)。
        let scale = pet.window().scale_factor() as f32;
        let pet_px = pet_size * scale;

        // 宠物中心(物理)=(窗口横向正中, 窗口底边往上 pet/2)。逐项说清楚:
        //
        // - **窗口位置/宽度用量的**(`platform::window_rect` 问系统要,不用 Slint
        //   缓存的那份 —— 它的尺寸要等 Resized 事件推上来才更新,连着改两次时
        //   第二次读到的还是旧的)。横向量到的中心**就是宠物现在待的地方**:
        //   宠物在窗口里居中,窗口横向怎么变它都不动,所以按量到的来不会被谁带偏。
        // - **高度用我们自己的 `old_h`,不用量到的**。桌宠的窗口并不只有我们在改:
        //   启动时那下 ±1px 重排、以及 Slint 自己(布局的**最小**尺寸比窗口高时
        //   它会直接把窗口撑大,只撑不缩)都会从左上角改高度。那些改动都会让
        //   「量到的底边」比真正的底边低,而底边是宠物唯一的落脚点 ——
        //   拿量到的当基准就会把错的位置固定下来(实测:宠物浮在离屏幕底 87px
        //   的地方,而且再也下不来)。
        let first = old_h <= 0.0;
        let live = platform::window_rect(platform::PET_TITLE);
        let (cx, cy) = match live {
            Some((x, y, lw, _)) => {
                (x as f32 + lw as f32 / 2.0, y as f32 + old_h * scale - pet_px / 2.0)
            }
            // 句柄没了:托盘里把桌宠关掉过 —— Slint 隐藏窗口时会**销毁** winit
            // 窗口,顺手把当时的位置尺寸记进 attributes(它自己也是这么记住的)。
            // 照那份摆,别把用户拖过去的位置丢了。
            None if !first => {
                let pos = pet.window().position();
                (
                    pos.x as f32 + pet.window().size().width as f32 / 2.0,
                    pos.y as f32 + old_h * scale - pet_px / 2.0,
                )
            }
            // 窗口还没建出来(启动时第一次调用):默认停在工作区右下角。
            // 这里必须把中心整个算出来,不能沿用窗口当时的坐标 —— 那时它还是 0,
            // 宠物会跑到屏幕左上角去。
            None => {
                let (wx, wy, ww, wh) = platform::work_area();
                let margin = PET_MARGIN as f32 * scale;
                (
                    (wx + ww) as f32 - margin - w * scale / 2.0,
                    (wy + wh) as f32 - margin - pet_px / 2.0,
                )
            }
        };
        self.pet_h.set(h);
        self.pet_w.set(w);

        let nx = (cx - w * scale / 2.0).round() as i32;
        let ny = (cy + pet_px / 2.0 - h * scale).round() as i32;
        let nw = (w * scale).round() as i32;
        let nh = (h * scale).round() as i32;

        // 没有句柄时(上面那两个 None 分支)只能走 Slint 的两次调用 —— 这一步还得
        // 让 Slint 记住这个尺寸:它建窗口用的就是 `has_explicit_size` 记下的那个数,
        // 不先设一次,窗口会按布局的推荐尺寸(220x140)建出来再被下面改一次。
        if live.is_some() && platform::set_window_rect(platform::PET_TITLE, nx, ny, nw, nh) {
            return;
        }
        pet.window().set_size(slint::LogicalSize::new(w, h));
        pet.window()
            .set_position(slint::LogicalPosition::new(nx as f32 / scale, ny as f32 / scale));
    }

    /// 双击宠物:把主窗口叫出来


    /// 单击桌宠。**延迟 250ms 才真的执行**,双击来了就把这次单击取消。
    ///
    /// 不这么做的话:双击的第一下会先把「快速添加」输入条打开,窗口随之变高,
    /// Slint 的双击判定就被这次尺寸变化打断,第二下只当成又一次单击 ——
    /// 表现是「双击打不开对话窗,反而开了个输入条」(这是实测到的现象)。
    fn pet_clicked(&self) {
        let Some(me) = self.me.borrow().upgrade() else { return };
        if self.pet_click_timer.borrow().is_none() {
            *self.pet_click_timer.borrow_mut() = Some(Timer::default());
        }
        if let Some(t) = self.pet_click_timer.borrow().as_ref() {
            t.start(TimerMode::SingleShot, Duration::from_millis(250), move || {
                me.toggle_pet_input()
            });
        }
    }

    /// 双击桌宠:开对话窗(并把挂起的那次单击取消掉)
    fn pet_double_clicked(&self) {
        if let Some(t) = self.pet_click_timer.borrow().as_ref() {
            t.stop();
        }
        self.open_chat();
    }

    /// 点未完成数角标(红点):直接看任务清单。
    ///
    /// 先把挂起的那次单击掐掉:红点和宠物用的是两个 TouchArea,按说只有上面那个
    /// 会收到事件,但万一两个都算成自己的,250ms 后还会弹出快速添加输入条 ——
    /// 刚打开的主窗口还没看清就被一条输入条顶上来,很怪。
    fn pet_badge_clicked(&self) {
        if let Some(t) = self.pet_click_timer.borrow().as_ref() {
            t.stop();
        }
        // 从红点上拖走再松手,也会走一次 clicked —— 那不是「点」。
        if self.take_pet_just_dragged() {
            return;
        }
        self.open_task_list();
    }

    /// 把主窗口叫出来,并且切到列表视图。
    ///
    /// 和托盘「打开」的区别:那个只把窗口叫出来、停在用户上次看的视图;这个还要切到
    /// 列表 —— 角标的含义就是「还有 N 条没做」,点它当然是来看这 N 条的。
    fn open_task_list(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        if self.view.get() != View::List {
            self.set_view(View::List);
        }
        if let Err(err) = self.reveal(&ui, platform::WINDOW_TITLE) {
            platform::log(&format!("显示主窗口失败: {err}"));
        }
        // 主窗口可能正被其它窗口盖着(尤其桌宠是 always-on-top):叫到前台才有意义,
        // 光 show() 不够。
        //
        // 这里**不能用 `bring_to_front`**:它内含 `SW_RESTORE`,会把最大化的主窗口
        // 缩回普通大小 —— 用户最大化着窗口,点个红点反而被缩了。还原最小化那件事
        // 已经由 `reveal` 处理(它只在真的最小化时才还原)。
        platform::focus_window(platform::WINDOW_TITLE);
    }

    /// 拖动松手时 TouchArea 也会发一次 `clicked`(松手时指针还在宠物上),
    /// 那不是「点一下」。取走并清掉这个标记,让调用方决定怎么办。
    fn take_pet_just_dragged(&self) -> bool {
        let was = self.pet_just_dragged.get();
        self.pet_just_dragged.set(false);
        was
    }

    /// 点宠物:展开 / 收起快速添加。
    ///
    /// **不再顺带换动画**(以前展开切「看书」、收起切回待机):点一下只该开个输入条,
    /// 动画交给待机轮播自己走。顺带修掉一个副作用 —— 那两下会把「这个待机住够多久」
    /// 的计时重置掉,点得多的时候轮播就再也轮不起来了。
    fn toggle_pet_input(&self) {
        if self.take_pet_just_dragged() {
            return;
        }
        let Some(pet) = self.pet.borrow().clone() else { return };
        let app = pet.global::<AppData>();
        let open = !self.pet_adding.get();
        self.pet_adding.set(open);

        // ⚠️ **先推「输入条开没开」还是先动那一圈,顺序是有讲究的。**
        //
        // 桌宠盒子的高度 = 宠物 + `pet-ring-band`,而输入条/气泡是**摆在它上面**的,
        // 所以「布局要的最小高度」= 输入条 + 间距 + 宠物盒子。这两样分两次推给界面,
        // 中间就会有一帧是「输入条开着 + 环的留白还没撤」—— 那时最小高度比窗口还高,
        // 而 **Slint 会把窗口撑到最小高度**(`adjust_window_size_to_satisfy_constraints`,
        // 从左上角撑,而且只撑不缩):实测窗口被撑到 278,底边从 1368 掉到 1392,
        // 宠物浮在屏幕底边上下不来了。
        //
        // 所以:**开**输入条时先把那一圈收掉(留白归 0),**关**的时候先把「关了」
        // 推出去,之后才轮到那一圈 —— 两头都不会出现「两样同时占着」的中间帧。
        if open {
            // 顺带把那一圈收掉:两样抢的是窗口顶上同一块位置。不在这儿收,就得等
            // 下一拍悬停判定(40ms)自己发现「忙」而收起来 —— 中间那些帧窗口高度是
            // 「环 + 输入条」两样都占着,一次点击等于连改两次高度。
            self.set_pet_ring(false);
            app.set_pet_adding(true);
        } else {
            // 收起时把草稿清掉 —— 留着的话下次展开会看到上次没发出去的内容
            app.set_pet_adding(false);
            app.set_pet_draft("".into());
            // 指针还在宠物/那一圈附近的话,**当场**把那一圈叫回来,别等下一拍悬停
            // 判定(40ms)。等的那一下里「输入条没了、环也还没开」,窗口会先缩回
            // 空闲那一档再长回来 —— 又是两次改高度。
            // 判据和 `tick_pet_ring` 共用同一个 `pet_hover`,不会打架。
            if !self.pet_ring_dismissed.get() {
                let (in_pet, in_ring) = self.pet_hover(&pet);
                if in_pet || in_ring {
                    self.set_pet_ring(true);
                }
            }
        }
        self.layout_pet_window();
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

    /// 把「开机自启」的**真实状态**推给设置窗口。
    ///
    /// 每次都去读注册表,而不是记住用户刚点的那个值:写注册表**可能失败**
    /// (企业策略、权限),失败时开关必须拨回去 —— 停在一个骗人的状态上,
    /// 用户下次开机发现没启动,会以为是自己没点。
    fn push_autostart(&self) {
        let enabled = platform::autostart_enabled();
        let settings = self.settings.borrow().as_ref().cloned();
        if let Some(settings) = settings {
            settings.global::<AppData>().set_autostart(enabled);
        }
    }

    fn set_autostart(&self, enabled: bool) {
        if let Err(err) = platform::set_autostart(enabled) {
            platform::log(&format!("设置开机自启失败: {err}"));
        }
        // 成功失败都按注册表的实际结果回推(失败了开关自动弹回去)
        self.push_autostart();
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
        platform::log(&format!("托盘动作: {action:?}"));

        match action {
            TrayAction::Open => {
                let _ = self.reveal(&ui, platform::WINDOW_TITLE);
            }
            TrayAction::Today => {
                // 先把视图和游标摆好,**再**把窗口亮出来:反过来的话第一帧画的是
                // 上次那个视图,窗口起来时能看见它翻一下页。
                self.set_view(View::Calendar);
                self.goto_today();
                let _ = self.reveal(&ui, platform::WINDOW_TITLE);
            }
            TrayAction::Settings => {
                self.open_settings();
            }
            TrayAction::TogglePet(on) => {
                self.show_pet.set(on);
                if let Some(pet) = self.pet.borrow().clone() {
                    if on {
                        self.layout_pet_window();
                        let _ = self.reveal(&*pet, platform::PET_TITLE);
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
    /// 显示窗口,必要时先把它从任务栏还原,然后让缓冲区重排一次。
    ///
    /// `title` 是窗口标题 —— 只用来问系统「这窗口最小化了吗」(见下)。
    fn reveal<W: slint::ComponentHandle + 'static>(
        &self,
        ui: &W,
        title: &str,
    ) -> Result<(), slint::PlatformError> {
        ui.show()?;

        // 最小化的窗口**必须绕开下面那个 ±1px 重排**:这时候 `Window::size()` 给的
        // 是「任务栏缩略图」的尺寸(实测 160x28),拿它去重排,窗口就真被缩成一小条,
        // 而且再也回不来(用户点托盘「打开」会看到主窗口变成一条)。
        //
        // 光把重排推迟也不行 —— 还原是同步生效的,但 Slint 缓存的尺寸要等 winit 把
        // 事件推上来才更新,当场读到的还是缩略图尺寸。所以:先还原,再**等一拍**
        // 重排,那时读到的才是真尺寸。
        if platform::is_minimized(title) {
            platform::restore_window(title);
            let weak = ui.as_weak();
            let Some(me) = self.me.borrow().upgrade() else { return Ok(()) };
            let timer = Timer::default();
            timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(250),
                move || {
                    if let Some(ui) = weak.upgrade() {
                        me.nudge_window(&ui);
                    }
                },
            );
            self.repaint_timers.borrow_mut().push(timer);
            return Ok(());
        }

        self.nudge_window(ui);
        Ok(())
    }

    /// 「±1px 顶一下再收回来」—— 逼 softbuffer 重新分配缓冲区,从而整窗口重画一次。
    ///
    /// 为什么需要它、以及为什么别的办法(请求重画、同一个 tick 里改两次尺寸)都不行,
    /// 见这一节开头的长注释。**最小化的窗口千万别调**(原因见 `State::reveal`)。
    fn nudge_window<W: slint::ComponentHandle + 'static>(&self, ui: &W) {
        // 换算成逻辑尺寸再改:set_size 收 LogicalSize,而 Window::size() 给的是物理的,
        // 直接拿物理数当逻辑用会在高 DPI 上把窗口缩小。
        let scale = ui.window().scale_factor();
        let size = ui.window().size().to_logical(scale);
        let nudged = slint::LogicalSize::new(size.width, size.height + 1.0);
        ui.window().set_size(nudged);

        let weak = ui.as_weak();
        let timer = Timer::default();
        timer.start(TimerMode::SingleShot, Duration::from_millis(120), move || {
            let Some(ui) = weak.upgrade() else { return };
            // **只有尺寸还是我们顶过的那一次,才收回来。**
            //
            // 这 120ms 里窗口完全可能因为别的原因改过大小 —— 桌宠就是:启动时鼠标
            // 正好停在宠物身上,那一圈图标弹出来,窗口得长高 64px。这时候要是拿
            // 120ms 前存下的旧尺寸设回去,等于把那次改动抹掉;而且 `set_size` 是从
            // **左上角**缩的,桌宠的内容贴着底边,底边就跟着往上跳 ——
            // 实测:宠物会浮在离屏幕底 87px 的地方,而且再也回不去(新位置被当成
            // 「用户放的地方」)。
            let now = ui.window().size().to_logical(ui.window().scale_factor());
            if (now.height - nudged.height).abs() < 0.5 {
                ui.window().set_size(size);
            }
        });
        // Timer drop 掉就停了,必须留个引用
        self.repaint_timers.borrow_mut().push(timer);
    }
}

/// Markdown 源码切成 UI 用的块。
///
/// 块级由 `markdown::parse` 切(标题/代码/引用/分隔线/表格),**行内样式交给
/// `StyledText::from_markdown`** —— Slint 1.18 的 `StyledText` 元素认 CommonMark 的
/// 粗体/斜体/删除线/行内代码/链接/列表,正好补上它自己不做的那一层。
///
/// 解析失败就退回纯文本:宁可这一块没有样式,也不能整段不显示。
fn styled(md: &str) -> slint::StyledText {
    slint::StyledText::from_markdown(md)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(md))
}

/// 空的单元格模型(非表格的块用它占位 —— Slint 的 `[MdCell]` 字段必须给个有效模型)
fn empty_cells() -> ModelRc<MdCell> {
    ModelRc::from(Rc::new(VecModel::<MdCell>::default()))
}

/// 图片的最大显示尺寸(逻辑像素)。
///
/// 不设上限的话,一张手机截图(1080×2400)能把预览撑成一条竖线,别的块全被挤到屏幕外。
/// **只缩不放** —— 小图放大会糊。
const IMAGE_MAX_W: f32 = 560.0;
const IMAGE_MAX_H: f32 = 520.0;

/// 语法高亮缓存的上限(条)。打字时每次改动都会产生一份新的 —— 不封顶会一直涨
/// (一段长代码就是几百个字符串),所以**超了直接全清**,不值当为它做 LRU。
const CODE_CACHE_MAX: usize = 64;

/// 代码块高亮好的行(一行 = 若干段同色文字),缓存里存的就是它
type CodeLines = Rc<Vec<MdCodeLine>>;

/// 一条图片缓存项
#[derive(Clone)]
enum ImageSlot {
    /// 解码好了:图 + **已经算好的显示尺寸**(等比缩进 IMAGE_MAX_* 之内)
    /// + 原始像素尺寸(看原图那个浮层要用)
    Ok { image: slint::Image, w: f32, h: f32, nat_w: f32, nat_h: f32 },
    /// 没成:记下原因,占位框要显示给人看
    Err(String),
}

/// 把 `![alt](路径)` 里的路径解析成磁盘上的绝对路径。
///
/// - 相对路径按**当前笔记所在目录**解析(`base`),绝对路径照用;
/// - `http(s)://` / `data:` / `file://` 都不支持 —— 网络图要下载+缓存,内嵌图要 base64,
///   这一轮都不做,给一句人话当原因。
///
/// 这里**故意不拦 `..`**:写笔记的是用户自己,他写 `../截图/a.png` 就是想去那个目录。
/// (笔记**名**的净化是另一回事,在 `notes::sanitize` 里 —— 那个挡的是名字,不是正文引用。)
fn resolve_image_path(base: Option<&Path>, url: &str) -> Result<PathBuf, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("路径是空的".into());
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Err("暂不支持网络图片".into());
    }
    if lower.starts_with("data:") {
        return Err("暂不支持内嵌(data:)图片".into());
    }
    if lower.starts_with("file://") {
        return Err("不支持 file:// 写法,直接写盘符路径就行".into());
    }
    let path = Path::new(url);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    match base {
        Some(dir) => Ok(dir.join(path)),
        None => Err("这里没有基准目录,相对路径解析不了".into()),
    }
}

/// 读盘 + 解码 + 算好显示尺寸。
///
/// 走的是和桌宠帧一样的 `load_from_data`(见 `pet.rs`),不是 `load_from_path` ——
/// 前者能拿到「读不到 / 解不开」的具体原因,占位框上要显示给人看。
/// **同步解码**(一张几 MB 的截图几十毫秒),所以外面必须套缓存,见 `State::image_slot`。
fn load_image_from(path: &Path) -> ImageSlot {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => return ImageSlot::Err(format!("读不到文件:{err}")),
    };
    // 扩展名当格式提示;认不出来就让 Slint 自己嗅探(它按内容认)
    let hint = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let image = match slint::Image::load_from_data(&bytes, hint.as_deref()) {
        Ok(image) => image,
        Err(err) => return ImageSlot::Err(format!("解不开这张图:{err}")),
    };
    let size = image.size();
    let (iw, ih) = (size.width as f32, size.height as f32);
    if iw <= 0.0 || ih <= 0.0 {
        return ImageSlot::Err("图是空的(尺寸 0)".into());
    }
    let scale = (IMAGE_MAX_W / iw).min(IMAGE_MAX_H / ih).min(1.0);
    ImageSlot::Ok {
        image,
        w: (iw * scale).round(),
        h: (ih * scale).round(),
        nat_w: iw,
        nat_h: ih,
    }
}

/// Markdown 的对齐 → UI 的整数编码(0 左 / 1 中 / 2 右,和 widgets.slint 的 MdCell 对齐)
fn align_code(align: markdown::Align) -> i32 {
    match align {
        markdown::Align::Left => 0,
        markdown::Align::Center => 1,
        markdown::Align::Right => 2,
    }
}

impl State {
    /// 把解析出来的块转成 Slint 侧的 `MdBlock`。
    ///
    /// 注意两类字段的用途(见 widgets.slint 里 MdBlock 的说明):
    /// `text` 是给 `StyledText` 的富文本,`raw` 是给 `Text` 的**原样纯文本** ——
    /// 代码块和表格必须走 raw,否则代码里的 `*` 会被当成强调标记吃掉。
    ///
    /// `base` 是**当前笔记所在目录**:正文里 `![](相对路径)` 要按它解析。
    /// 对话窗那边没有基准目录,传 `None`(那里的本地图只会显示成占位框)。
    fn to_ui_block(&self, block: markdown::Block, base: Option<&Path>) -> MdBlock {
        let s = SharedString::from;
        match block {
            markdown::Block::Rich(text) => MdBlock {
                kind: MdKind::Rich,
                text: styled(&text),
                raw: s(&text),
                cells: empty_cells(),
                ..Default::default()
            },
            markdown::Block::Heading { level, text } => MdBlock {
                kind: MdKind::Heading,
                text: styled(&text),
                raw: s(&text),
                level: level as i32,
                cells: empty_cells(),
                ..Default::default()
            },
            markdown::Block::Code { lang, code } => MdBlock {
                kind: MdKind::Code,
                raw: s(&code),
                lang: s(&lang),
                // 语法高亮:按行、按段切好推给界面(配色在 Slint 那边给)
                lines: ModelRc::from(Rc::new(VecModel::from((*self.code_lines(&lang, &code)).clone()))),
                cells: empty_cells(),
                ..Default::default()
            },
            markdown::Block::Quote(text) => MdBlock {
                kind: MdKind::Quote,
                text: styled(&text),
                raw: s(&text),
                cells: empty_cells(),
                ..Default::default()
            },
            markdown::Block::Rule => MdBlock {
                kind: MdKind::Rule,
                cells: empty_cells(),
                ..Default::default()
            },
            markdown::Block::Table { rows, aligns } => {
                // 拍平成一维 + 带行列号:Slint 那边要用 GridLayout 的 row/column 摆
                let cells: Vec<MdCell> = rows
                    .iter()
                    .enumerate()
                    .flat_map(|(r, row)| {
                        let aligns = aligns.clone();
                        row.iter().enumerate().map(move |(c, text)| MdCell {
                            text: s(text),
                            row: r as i32,
                            col: c as i32,
                            header: r == 0,
                            align: aligns.get(c).copied().map(align_code).unwrap_or(0),
                        })
                    })
                    .collect();
                MdBlock {
                    kind: MdKind::Table,
                    cells: ModelRc::from(Rc::new(VecModel::from(cells))),
                    // 列数给 UI:列宽要均分(Slint 里数一维模型很别扭,这里顺手带过去)
                    table_cols: rows.first().map(|r| r.len()).unwrap_or(0) as i32,
                    ..Default::default()
                }
            }
            markdown::Block::Image { alt, url } => {
                let label = if alt.trim().is_empty() { url.clone() } else { alt };
                let (ok, image, w, h, nat_w, nat_h, detail) =
                    match resolve_image_path(base, &url) {
                        Ok(path) => match self.image_slot(&path) {
                            ImageSlot::Ok { image, w, h, nat_w, nat_h } => {
                                (true, image, w, h, nat_w, nat_h, String::new())
                            }
                            ImageSlot::Err(err) => (
                                false,
                                slint::Image::default(),
                                0.0,
                                0.0,
                                0.0,
                                0.0,
                                format!("{}\n{err}", path.display()),
                            ),
                        },
                        Err(err) => (
                            false,
                            slint::Image::default(),
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                            format!("{url}\n{err}"),
                        ),
                    };
                MdBlock {
                    kind: MdKind::Image,
                    image,
                    image_ok: ok,
                    image_width: w,
                    image_height: h,
                    image_natural_width: nat_w,
                    image_natural_height: nat_h,
                    image_label: s(&label),
                    image_detail: s(&detail),
                    cells: empty_cells(),
                    ..Default::default()
                }
            }
            markdown::Block::Tasks(items) => {
                let tasks: Vec<MdTask> = items
                    .iter()
                    .map(|t| MdTask { done: t.done, text: styled(&t.text), line: t.line as i32 })
                    .collect();
                MdBlock {
                    kind: MdKind::Tasks,
                    tasks: ModelRc::from(Rc::new(VecModel::from(tasks))),
                    cells: empty_cells(),
                    ..Default::default()
                }
            }
        }
    }

    /// 代码块的语法高亮,**带缓存**。
    ///
    /// 缓存不是优化而是必需:对话流式那条路每个 tick 都会把整段回复重新切块
    /// (见 `poll_chat`,约 20 次/秒),没缓存的话同一段代码每秒要解析几十次 ——
    /// tree-sitter 解析 + 跑 query 是毫秒级的活儿。
    ///
    /// 键里带**代码本身的哈希**(同一段代码改了就是另一份)。认不出的语言走
    /// `highlight::highlight` 里的单色退化,不报错。
    fn code_lines(&self, lang: &str, code: &str) -> CodeLines {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut hasher);
        let key = (lang.trim().to_ascii_lowercase(), hasher.finish());

        if let Some(hit) = self.code_cache.borrow().get(&key) {
            return hit.clone();
        }

        let lines: Vec<MdCodeLine> = highlight::highlight(lang, code)
            .into_iter()
            .map(|spans| {
                let spans: Vec<MdCodeSpan> = spans
                    .into_iter()
                    .map(|s| MdCodeSpan { text: s.text.as_str().into(), class: s.class.code() })
                    .collect();
                MdCodeLine { spans: ModelRc::from(Rc::new(VecModel::from(spans))) }
            })
            .collect();
        let lines = Rc::new(lines);

        let mut cache = self.code_cache.borrow_mut();
        if cache.len() >= CODE_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, lines.clone());
        lines
    }

    /// 按路径取图,**带缓存**。
    ///
    /// 缓存不是优化而是必需:对话流式那条路每个 tick 都会把整段回复重新切块
    /// (见 `poll_chat`,约 20 次/秒),没缓存的话同一张图每秒要解几十次。
    /// 键里带**文件修改时间** —— 换了图重开预览就能看到新的,不用重启进程。
    fn image_slot(&self, path: &Path) -> ImageSlot {
        let stamp: Option<SystemTime> = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let key = (path.to_path_buf(), stamp);
        if let Some(hit) = self.image_cache.borrow().get(&key) {
            return hit.clone();
        }
        let slot = load_image_from(path);
        self.image_cache.borrow_mut().insert(key, slot.clone());
        slot
    }

    /// 把一段 Markdown 正文切成 UI 用的块模型。
    ///
    /// 空文本给空模型:Slint 的 `[MdBlock]` 字段从 Rust 侧必须是个有效的 ModelRc,
    /// 不能留 undefined。
    ///
    /// ⚠️ 对话那条路**每个 tick 都重建一次**(流式回复),所以图片解码必须吃缓存。
    fn chat_blocks(&self, markdown: &str) -> ModelRc<MdBlock> {
        let blocks: Vec<MdBlock> = if markdown.trim().is_empty() {
            Vec::new()
        } else {
            markdown::parse(markdown).into_iter().map(|b| self.to_ui_block(b, None)).collect()
        };
        ModelRc::from(Rc::new(VecModel::from(blocks)))
    }
}

/// 后台线程:发请求、读流、把增量塞进 channel。
///
/// **这里绝对不能碰任何 Slint 对象** —— 跨线程访问 UI 会 panic 或数据竞争。
/// 所有界面更新都在主线程的 `State::poll_chat` 里做。
fn run_chat(
    cfg: model::AiConfig,
    messages: Vec<ai::Message>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<ChatEvent>,
) {
    use std::sync::atomic::Ordering;

    let url = ai::endpoint(&cfg.base_url);
    let agent = ureq::Agent::config_builder()
        // 非 2xx 不当异常抛出:要能把服务端返回的那段 JSON 读出来给人看,
        // 不然用户只能看到「请求失败」而不知道是密钥错了还是模型名错了
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // 兜底总时长。**它不是空闲超时** —— ureq 文档写明了「预算不按每次读重置」,
        // 所以给得宽松些;真想中断,用户点停止(那是另一条路,见 cancel)。
        .timeout_recv_body(Some(Duration::from_secs(300)))
        .build()
        .new_agent();

    let body = ai::request_body(&cfg.model, &messages, true);
    let sent = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        // Azure OpenAI 认的是这个头(它不认 Bearer);OpenAI 和中转站会忽略多余的头。
        // 两个一起发,用户就不用先搞清楚自己用的是哪一派。
        .header("api-key", &cfg.api_key)
        .header("Accept", "text/event-stream")
        .send_json(&body);

    let resp = match sent {
        Ok(r) => r,
        Err(err) => {
            let _ = tx.send(ChatEvent::Failed(ai::friendly_error(&err.to_string())));
            return;
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 不是流(或者压根是错误响应):整段读出来按普通 JSON 解析。
    // 两种情况都会走到这儿:① 有的服务无视 stream:true 直接给完整响应;
    // ② 密钥/模型/额度出错时返回的是一个 JSON 错误体而不是 SSE。
    if !status.is_success() || !ai::is_event_stream(&content_type) {
        let mut resp = resp;
        let raw = resp.body_mut().read_to_string().unwrap_or_default();
        match ai::parse_full_response(&ai::truncate(&raw, 2000)) {
            Ok(answer) => {
                let _ = tx.send(ChatEvent::Delta(answer));
                let _ = tx.send(ChatEvent::Done);
            }
            Err(msg) => {
                let _ = tx.send(ChatEvent::Failed(ai::friendly_error(&format!(
                    "HTTP {status}: {msg}"
                ))));
            }
        }
        return;
    }

    // SSE:逐行读。
    //
    // **用 lines() 而不是自己按 chunk 切**:网络分块不会对齐到换行,一个
    // `data: {...}` 完全可能被拆在两个 TCP 包里。参考项目里直接 `split("\n\n")`
    // 就是这么出 bug 的(而且很难复现)。lines() 自带缓冲,正好解决这件事。
    let reader = resp.into_body().into_reader();
    for line in std::io::BufReader::new(reader).lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(ChatEvent::Failed("已中断".into()));
            return;
        }
        let Ok(line) = line else { break }; // 读断了就收尾,别把半截当成功
        match ai::parse_sse_line(&line) {
            ai::Chunk::Text(t) => {
                if tx.send(ChatEvent::Delta(t)).is_err() {
                    return; // 主线程那边不要了(窗口关了),直接收工
                }
            }
            ai::Chunk::Done => break,
            ai::Chunk::Error(e) => {
                let _ = tx.send(ChatEvent::Failed(ai::friendly_error(&e)));
                return;
            }
            ai::Chunk::Ignore => {}
        }
    }
    let _ = tx.send(ChatEvent::Done);
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

    // ⚠️ **这两件必须放在最前面,连日志都不能先写**:日志、待办、笔记、AI 配置
    // 全在数据目录下,晚一步就会先在新目录里建出文件来,那时搬迁会直接放弃
    // (见 model::migrate_old_data_dirs 的说明)—— 表现就是「任务和笔记全没了」。
    model::migrate_old_data_dirs();
    platform::migrate_autostart();

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
        pet_w: Cell::new(0.0),
        pet_ring: Cell::new(false),
        pet_ring_dismissed: Cell::new(false),
        repaint_timers: RefCell::new(Vec::new()),
        chat: RefCell::new(None),
        chat_model: Rc::new(VecModel::default()),
        chat_history: RefCell::new(Vec::new()),
        chat_busy: Cell::new(false),
        chat_status: RefCell::new(String::new()),
        chat_cancel: RefCell::new(None),
        chat_rx: RefCell::new(None),
        chat_timer: RefCell::new(None),
        chat_scroll_tick: Cell::new(0),
        chat_config_open: Cell::new(false),
        notes: RefCell::new(None),
        notes_dir: notes::notes_dir(),
        notes_current: RefCell::new(String::new()),
        notes_model: Rc::new(VecModel::default()),
        // 默认分屏 + 显示列表:进来先看得见「有哪几篇」,再决定要不要铺满
        notes_view: Cell::new(MdView::Split),
        notes_list_shown: Cell::new(true),
        notes_dirty: Cell::new(false),
        notes_status: RefCell::new(String::new()),
        notes_blocks: Rc::new(VecModel::default()),
        notes_last_source: RefCell::new(String::new()),
        image_cache: RefCell::new(HashMap::new()),
        code_cache: RefCell::new(HashMap::new()),
        notes_tick: Cell::new(0),
        notes_timer: RefCell::new(None),
        pet_click_timer: RefCell::new(None),
        chat_cfg: RefCell::new(model::load_ai(&model::ai_config_file())),
        chat_cfg_path: model::ai_config_file(),
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
                // 亮暗和外观一样挂在设置窗口上。宿主只写主窗口那份 Theme,
                // 其余窗口由 push_theme 推 —— 设置窗口里那两个 Chip 因此立刻跟着变。
                let s = state.clone();
                settings.global::<Logic>()
                    .on_set_theme_preference(move |value| s.set_theme_preference(value));
            }
            {
                // 开机自启原来在主窗口底栏,和「外观 / 亮暗」一起收进设置里了
                let s = state.clone();
                settings.global::<Logic>().on_set_autostart(move |on| s.set_autostart(on));
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
                    pet.global::<Logic>().on_pet_clicked(move || s.pet_clicked());
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>()
                        .on_pet_double_clicked(move || s.pet_double_clicked());
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>()
                        .on_pet_badge_clicked(move || s.pet_badge_clicked());
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>()
                        .on_pet_ring_action(move |action| s.pet_ring_action(&action));
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_add_quick_task(move |title| s.add_quick_task(&title));
                }
                {
                    let s = state.clone();
                    pet.global::<Logic>().on_open_chat(move || s.open_chat());
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

    // ── AI 对话窗口 ──
    // 又是独立窗口,同样有自己那份 AppData / Logic,回调要单独接。
    match ChatWindow::new() {
        Ok(chat) => {
            {
                let s = state.clone();
                chat.global::<Logic>().on_chat_send(move |text| s.start_chat(&text));
            }
            {
                let s = state.clone();
                chat.global::<Logic>().on_chat_cancel(move || s.cancel_chat());
            }
            {
                let s = state.clone();
                chat.global::<Logic>().on_chat_close(move || s.close_chat());
            }
            {
                let s = state.clone();
                chat.global::<Logic>()
                    .on_chat_toggle_config(move || s.toggle_chat_config());
            }
            {
                let s = state.clone();
                chat.global::<Logic>().on_chat_save_config(move |base, key, model| {
                    s.save_chat_config(&base, &key, &model)
                });
            }
            {
                // 对话窗标题栏那个「笔记」按钮
                let s = state.clone();
                chat.global::<Logic>().on_md_open(move || s.open_notes());
            }
            {
                // AI 回复旁边的「存到笔记」
                let s = state.clone();
                chat.global::<Logic>()
                    .on_md_append(move |text| s.append_to_notes(&text));
            }
            // 点 ✕ 只是藏起来(聊天记录留着)
            chat.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);
            *state.chat.borrow_mut() = Some(Rc::new(chat));
        }
        Err(err) => platform::log(&format!("创建对话窗口失败,双击桌宠将没反应: {err}")),
    }

    // ── Markdown 草稿本窗口 ──
    match NotesWindow::new() {
        Ok(notes) => {
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_save(move || s.save_notes());
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_close(move || s.close_notes());
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_open(move || s.open_notes());
            }
            // 笔记库:换一篇 / 新建 / 改名 / 删除 / 换视图 / 收列表
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_select(move |name| s.select_note(&name));
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_new(move || s.new_note());
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_rename(move |old, new| s.rename_note(&old, &new));
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_delete(move |name| s.delete_note(&name));
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_set_view(move |view| s.set_notes_view(view));
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_toggle_list(move || s.toggle_notes_list());
            }
            {
                let s = state.clone();
                notes.global::<Logic>().on_md_toggle_task(move |line| s.toggle_note_task(line));
            }
            // 点 ✕ 也走「先存再藏」那条路(别让用户白写)
            {
                let s = state.clone();
                notes.window().on_close_requested(move || {
                    s.save_notes();
                    slint::CloseRequestResponse::HideWindow
                });
            }
            *state.notes.borrow_mut() = Some(Rc::new(notes));
        }
        Err(err) => platform::log(&format!("创建笔记窗口失败: {err}")),
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
    //
    // ⚠️ **这里故意不显示主窗口**(`state.reveal(&ui, ...)` 被拿掉了):
    // 桌宠才是入口,任务清单/笔记/设置都从它悬停弹出来的那一圈进(见 §28)。
    // AppWindow 照建不误 —— 所有回调都捕获了它,只是先不上屏。
    // 第一次真正显示时走的是同一个 `reveal`,所以「启动只画一半」那个修法
    // (§23.1)照常生效,它本来就是为「第一次映射」写的。

    // 桌宠:先摆好位置再显示,免得先在屏幕中间闪一下
    if let Some(pet) = state.pet.borrow().clone().filter(|_| state.show_pet.get()) {
        state.push_pet();
        state.layout_pet_window();
        if let Err(err) = state.reveal(&*pet, platform::PET_TITLE) {
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
