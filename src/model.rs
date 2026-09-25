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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppearanceName {
    Solid,
    /// 玻璃:老数据文件没有这个字段时 serde 会填 default,升上来不会突然变样。
    Glass,
    /// 工业风:全直角 + 发丝线 + 纸墨中性色 + 信号黄。
    ///
    /// ⚠️ 这是**当前选定的缺省外观**,所以 `#[serde(default)]` 和 `defaults()`
    /// 都会落到它身上(见 `defaults()` 的注释)。
    #[default]
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
    /// 这个字段收不到有用的东西:`CURRENT_VERSION` 是 2、而 `i32::default()` 是 0,
    /// 靠 derive 的话第一份数据落盘就写着 `version: 0`。**新数据一律走
    /// `defaults()`**(它把 version 填对),所以这里只是给 serde 一个占位。
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
    // ⚠️ 下面 `pet_*` 这几项的缺省值就是 `defaults()` 里那个初始档(见该函数的注释)。
    // 加字段时记得两边一起改,别只在 defaults() 里加、忘了这里的 serde default ——
    // 那样「新装」和「老文件缺字段」会拿到不一样的值。
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
    /// **关掉**的动画名(设置窗口里每行那个勾选框)。
    ///
    /// 存「关掉的」而不是「开着的」是故意的:以后往 `assets/pet/` 丢新素材,
    /// 不用改存档它也自动参与轮播 —— 反过来存「开着的」,新素材加进来是关着的,
    /// 用户还得自己去找出来打开。
    #[serde(default)]
    pub pet_anim_off: std::collections::HashSet<String>,
    /// 动画的显示/轮播顺序(名字数组)。**空 = 按 `manifest.json` 的顺序**。
    ///
    /// 和 `pet_speeds` 一样带 serde default:老文件读出来是空,新字段不用迁移
    /// (理由见上面 appearance 那条)。
    #[serde(default)]
    pub pet_anim_order: Vec<String>,
    /// 跟随鼠标:鼠标静止一段时间后,宠物自己慢慢走过去(§40)。
    ///
    /// 存的是**总开关**;「哪些动画期间才允许跟随」见 `pet_follow_off`。
    #[serde(default = "default_true")]
    pub pet_follow: bool,
    /// 鼠标要静止多少秒才出发(设置里一根 3~60 秒的滑杆)
    #[serde(default = "default_follow_delay")]
    pub pet_follow_delay: f32,
    /// 走路速度**倍率**(1 = 基准 220 像素/秒)。和每个动画那根速度滑杆是同一个说法。
    #[serde(default = "default_follow_speed")]
    pub pet_follow_speed: f32,
    /// **不允许跟随**的动画名,缺省 = 只放行 `PET_FOLLOW_ALLOW` 那几个
    /// (那是这张表的补集,见 `PET_FOLLOW_OFF_DEFAULT`)。
    #[serde(default = "default_follow_off")]
    pub pet_follow_off: std::collections::HashSet<String>,
}

fn default_follow_delay() -> f32 {
    PET_FOLLOW_DELAY_DEFAULT
}

fn default_follow_speed() -> f32 {
    PET_FOLLOW_SPEED_DEFAULT
}

fn default_true() -> bool {
    true
}

fn default_pet_size() -> f32 {
    PET_SIZE_DEFAULT
}

fn default_follow_off() -> std::collections::HashSet<String> {
    PET_FOLLOW_OFF_DEFAULT.iter().map(|s| s.to_string()).collect()
}

/// **第一份数据长什么样。**
///
/// ⚠️ 这个函数定义的是「新装」(或数据文件丢了)时的整份缺省档。**别**在这里写
/// `todos`:清单必须是空的 —— 这里加一条,每个新用户开机就白捡一条待办。
///
/// 它和 `DataFile` 上那一堆 `#[serde(default = ...)]` 是**同一套缺省的两个入口**:
/// 前者管「整个文件不存在」,后者管「文件在、但缺某个字段」(老版本存上去的)。
/// **两边必须给同一个值** —— 不然同样的设置,新装和从老版本升上来会长得不一样。
/// 现在字段级的 default 全部转发到下面那些 `*_DEFAULT` 常量,就是为了只有一个源头。
pub fn defaults() -> DataFile {
    DataFile {
        version: CURRENT_VERSION,
        todos: Vec::new(),
        theme: ThemeName::default(),
        appearance: AppearanceName::Industrial,
        show_pet: true,
        pet_size: PET_SIZE_DEFAULT,
        pet_speeds: std::collections::HashMap::new(),
        pet_anim_off: std::collections::HashSet::new(),
        pet_anim_order: Vec::new(),
        pet_follow: true,
        pet_follow_delay: PET_FOLLOW_DELAY_DEFAULT,
        pet_follow_speed: PET_FOLLOW_SPEED_DEFAULT,
        pet_follow_off: default_follow_off(),
    }
}

// ── 桌宠外观参数(设置窗口可调,存盘)────────────────────────────────
//
// 这几个 `*_DEFAULT` 就是**当前选定的缺省档**(见 `defaults()` 的注释)。
// 范围卡在下面的上下限里,读盘时还会 clamp 一次 —— 手改 JSON 改出个 10000,
// 不该让宠物撑满屏幕。

pub const PET_SIZE_DEFAULT: f32 = 140.5;
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

/// 跟随鼠标:静止多久才出发(秒)。上限给到 60,再久就没人等得到了。
pub const FOLLOW_DELAY_MIN: f32 = 3.0;
pub const FOLLOW_DELAY_MAX: f32 = 60.0;
pub const FOLLOW_DELAY_DEFAULT: f32 = 10.0;

/// ⚠️ 下面这两个是**跟随鼠标那一组**的缺省档,和上面那对上下限是两回事 ——
/// 上面 `FOLLOW_DELAY_DEFAULT` 只用来给 `clamp_follow_delay` 兜底(读到非法值时
/// 退回哪),这里才是「界面上滑杆一开始停在哪」。改缺省档改这两个。
pub const PET_FOLLOW_DELAY_DEFAULT: f32 = 9.895161;
pub const PET_FOLLOW_SPEED_DEFAULT: f32 = 0.5826613;

/// **允许跟随的动画白名单。**
///
/// 和 `pet_anim_off` / `pet_follow_off` 平时那套「存关掉的、缺省=允许」**反过来**:
/// 这里是穷举,不在表里的动画默认**不跟随**(见 `PET_FOLLOW_OFF_DEFAULT`)。
///
/// 这么定是因为实际用下来「只有这几个姿势适合走来走去」,而不是「少数几个不适合」。
/// 代价:加新素材默认不会跟随,想让它跟随就加进这张表。
///
/// ⚠️ **运行时真正读的是 `PET_FOLLOW_OFF_DEFAULT`(它的补集),不是这张表** ——
/// 这张表是「意图」的单一出处,靠单测核对两边互补。所以非测试构建下它是 dead_code,
/// 那是预期内的,别顺手删掉(删了「哪些该跟随」就没地方声明了)。
#[cfg_attr(not(test), allow(dead_code))]
pub const PET_FOLLOW_ALLOW: &[&str] = &["idle-3", "idle-5", "idle-7"];

/// 缺省**不允许**跟随的动画 —— 就是上面那张表的补集。
///
/// ⚠️ **故意把补集也写死一份**:`ANIMATIONS` 是运行时才有的切片,const 期推不出补集来。
/// 所以两处必须一起改 —— 单测 `default_follow_off_is_exactly_the_complement_of_the_allow_list`
/// 会核对它们确实是互补的,加了素材忘了改就会红。
pub const PET_FOLLOW_OFF_DEFAULT: &[&str] = &[
    "idle-1", "idle-2", "idle-4", "idle-6", "idle-8", "read", "shop",
];

pub fn clamp_follow_delay(v: f32) -> f32 {
    if v.is_finite() { v.clamp(FOLLOW_DELAY_MIN, FOLLOW_DELAY_MAX) } else { FOLLOW_DELAY_DEFAULT }
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

/// 应用在用户数据目录下的文件夹名。
///
/// **改名时这个要跟着改**,并且把老名字登记到 `OLD_APP_DIRS` 里 ——
/// 不然老用户的任务和笔记会「凭空消失」(东西其实还在老文件夹里躺着)。
const APP_DIR: &str = "nuntel";

/// 以前用过的文件夹名。启动时会把第一个还在的整个搬过来。
const OLD_APP_DIRS: &[&str] = &["rgui-todo"];

/// 用户数据目录的父级(`%APPDATA%`;非 Windows 上退回 XDG/HOME)
fn data_root() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 数据目录:`%APPDATA%\nuntel\`。
///
/// **这里定义了这个目录里都有什么**(todos / 笔记库 / AI 配置 / 日志),
/// 别的地方要路径就调这几个函数,不要自己拼。
pub fn data_dir() -> PathBuf {
    data_root().join(APP_DIR)
}

/// 待办数据文件
pub fn data_file() -> PathBuf {
    data_dir().join("todos.json")
}

/// 运行日志。放在这里而不是 platform.rs:日志就是数据目录里的一个文件,
/// 目录布局只该有一处定义(`migrate_old_data_dirs` 也要按同一个名字找它)。
pub fn log_file() -> PathBuf {
    data_dir().join(format!("{APP_DIR}.log"))
}

/// 把老版本的数据目录整个搬成新的。**改名专用,和业务无关。**
///
/// ⚠️ **必须在任何读写之前调用**(`main()` 的第一件事)。日志、待办、笔记、
/// AI 配置全在这个目录下,晚一步就会先在新目录里建出文件来,那时 `new.exists()`
/// 为真、搬迁直接放弃 —— 用户看到的就是「我的任务和笔记全没了」。
///
/// 用 `rename` 而不是逐个文件复制:同一个父目录下它是原子的,而且**搬完老目录就没了,
/// 天然只生效一次**,不需要额外的「搬过了」标记(笔记库那个老单文件迁移也是这个路子)。
pub fn migrate_old_data_dirs() {
    let new = data_dir();
    for old_name in OLD_APP_DIRS {
        let old = data_root().join(old_name);
        if !old.is_dir() {
            continue;
        }
        if new.exists() {
            // 两边都在就**不动**:里面可能都有东西,自动合并只会把数据搞乱。
            // 说清楚让用户自己搬,比猜他要保留哪份强。
            crate::platform::log(&format!(
                "{} 和 {} 都存在,不自动搬迁 —— 需要保留旧数据的话请手工挪过来",
                old.display(),
                new.display()
            ));
            continue;
        }
        match std::fs::rename(&old, &new) {
            Ok(()) => {
                rename_log_inside(&new, old_name);
                crate::platform::log(&format!(
                    "数据目录已从 {} 搬到 {}",
                    old.display(),
                    new.display()
                ));
            }
            // 搬不动就按新目录跑,顶多是「看起来像新装」,总比起不来强
            Err(err) => crate::platform::log(&format!("搬数据目录失败,这次按新目录跑: {err}")),
        }
    }
}

/// 日志文件跟着改个名,免得新目录里躺着一个名字对不上的旧日志。
/// 失败无所谓 —— 日志本来就是排查用的,不影响任何功能。
fn rename_log_inside(new_dir: &Path, old_name: &str) {
    let old_log = new_dir.join(format!("{old_name}.log"));
    if old_log.is_file() {
        let _ = std::fs::rename(&old_log, new_dir.join(format!("{APP_DIR}.log")));
    }
}

/// 读盘。文件不存在或内容坏了都当空清单处理,不让应用起不来。
///
/// 顺带做一次 v1 → v2 的备份:老文件在写回之前先留一份 `todos.json.v1.bak`。
pub fn load(path: &Path) -> DataFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return defaults(); // 第一次运行还没这个文件
    };
    let data: DataFile = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(err) => {
            crate::platform::log(&format!("{} 内容无法解析,先当空清单处理: {err}", path.display()));
            return defaults();
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
    fn new_install_and_missing_field_agree_on_defaults() {
        // 两条入口:整个文件不存在(走 defaults()),和文件在但缺字段(走 serde default)。
        // 它们必须给同一套值,否则同样的设置「新装」和「从老版本升上来」会长得不一样。
        let from_serde: DataFile = serde_json::from_str("{}").unwrap();
        let fresh = defaults();

        assert_eq!(from_serde.appearance, fresh.appearance);
        assert_eq!(from_serde.show_pet, fresh.show_pet);
        assert_eq!(from_serde.pet_size, fresh.pet_size);
        assert_eq!(from_serde.pet_follow, fresh.pet_follow);
        assert_eq!(from_serde.pet_follow_delay, fresh.pet_follow_delay);
        assert_eq!(from_serde.pet_follow_speed, fresh.pet_follow_speed);
        assert_eq!(from_serde.pet_follow_off, fresh.pet_follow_off);
        assert_eq!(from_serde.pet_anim_off, fresh.pet_anim_off);
        assert_eq!(from_serde.pet_anim_order, fresh.pet_anim_order);
        assert_eq!(from_serde.pet_speeds, fresh.pet_speeds);
        assert_eq!(from_serde.theme, fresh.theme);
    }

    #[test]
    fn default_follow_off_is_exactly_the_complement_of_the_allow_list() {
        // 允许表里的名字必须真的存在(改名或删素材时会在这里炸出来)
        for name in PET_FOLLOW_ALLOW {
            assert!(
                crate::pet::ANIMATIONS.iter().any(|a| a.name == *name),
                "PET_FOLLOW_ALLOW 里的 {name} 在素材清单里不存在"
            );
        }
        // THE constraint:follow_off 必须正好是允许表的补集。
        // 少一个 = 那个动画默认会跟随,多一个 = 它被允许却又不跟随。
        let off = default_follow_off();
        let expected: std::collections::HashSet<String> = crate::pet::ANIMATIONS
            .iter()
            .map(|a| a.name)
            .filter(|n| !PET_FOLLOW_ALLOW.contains(n))
            .map(String::from)
            .collect();
        assert_eq!(off, expected, "follow_off 和 PET_FOLLOW_ALLOW 对不上");
        // 顺带把「所有动画恰好分成两堆」这个性质测掉
        assert_eq!(off.len() + PET_FOLLOW_ALLOW.len(), crate::pet::ANIMATIONS.len());
    }

    #[test]
    fn fresh_install_has_current_version_and_no_todos() {
        let fresh = defaults();
        assert_eq!(fresh.version, CURRENT_VERSION);
        // 缺省档里绝不能带待办,否则每个新用户开机就白捡一条
        assert!(fresh.todos.is_empty());
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
