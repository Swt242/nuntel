//! Windows 平台相关的杂活:单实例、系统通知、托盘、开机自启、把已有窗口叫到前台。
//!
//! 这些都没法用 Slint 的跨平台 API 表达,集中放这里,别的地方不直接碰 Win32。
//! 非 Windows 平台下各函数退化为「什么都不做」,保证代码还能编过。

use std::sync::OnceLock;

/// 主窗口标题。跟 `ui/app.slint` 里 `AppWindow.title` 保持一致 ——
/// 单实例要用它去找已经开着的那个窗口。
pub const WINDOW_TITLE: &str = "待办清单";

/// 设置窗口标题。跟 `ui/settings.slint` 里 `SettingsWindow.title` 一致。
///
/// **故意带上应用名**:`FindWindowW` 是按标题全局找的,单叫「设置」会撞上
/// 别的程序里同名的窗口。窗口是无边框的,这个标题不会显示出来,取多长都无所谓。
pub const SETTINGS_TITLE: &str = "待办清单 · 设置";

/// 桌宠窗口标题。跟 `ui/pet.slint` 里 `PetWindow.title` 一致。
/// 同样带上应用名避免和别的程序撞名,而且它也不进任务栏,标题不会被看到。
pub const PET_TITLE: &str = "待办清单 · 桌宠";
/// AI 对话窗口的标题(用来 FindWindow 找窗口)
pub const CHAT_TITLE: &str = "待办清单 · 助手";

/// Markdown 草稿本窗口的标题。跟 `ui/notes.slint` 里 `NotesWindow.title` 一致。
pub const NOTES_TITLE: &str = "待办清单 · 笔记";

/// 开机自启在注册表里的值名
const AUTOSTART_VALUE: &str = "rgui-todo";

/// 日志文件超过这个大小就在启动时清空,免得无限长
const LOG_MAX_BYTES: u64 = 256 * 1024;

/// 记一条日志。
///
/// 为什么不能只 `eprintln!`:release 构建带了 `windows_subsystem = "windows"`,
/// 根本没有控制台,stderr 直接丢掉 —— 出问题时(通知没弹、自启写失败)什么都看不到。
/// 所以除了打 stderr,还会追加到数据目录下的 rgui-todo.log。
pub fn log(message: &str) {
    eprintln!("{message}");

    let Some(path) = crate::model::data_file().parent().map(|dir| dir.join("rgui-todo.log"))
    else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(file, "{now} {message}");
    }
}

/// 启动时调一次:日志太大就清掉
pub fn rotate_log_if_needed() {
    let Some(path) = crate::model::data_file().parent().map(|dir| dir.join("rgui-todo.log"))
    else {
        return;
    };
    if std::fs::metadata(&path).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
}

// ── 单实例 ────────────────────────────────────────────────────────────

/// 拿住进程级互斥体。**必须活到进程结束**,所以拿到就泄漏掉。
///
/// 返回 `false` 表示已经有一个实例在跑,调用方应该把已有窗口叫到前台然后退出。
pub fn acquire_single_instance() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        static HANDLE: OnceLock<usize> = OnceLock::new();
        let name: Vec<u16> = "rgui-todo-single-instance\0".encode_utf16().collect();
        // SAFETY: 传的是合法的以 0 结尾的宽字符串;句柄故意不关闭,活到进程结束。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        // SAFETY: 紧跟 CreateMutexW 之后调用,读的是同一次系统调用的结果。
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if handle.is_null() {
            return true; // 拿不到互斥体就别挡着人家启动
        }
        let _ = HANDLE.set(handle as usize);
        !already_running
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// 把已经开着的那个窗口叫到前台(第二个实例启动时用)。
pub fn focus_existing_window() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
        };

        let title: Vec<u16> = WINDOW_TITLE.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: 标题是合法的以 0 结尾的宽字符串;找不到窗口时返回空句柄,下面直接跳过。
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
        }
    }
}

// ── 无边框窗口的窗口控制 ──────────────────────────────────────────────

/// 按标题精确找顶层窗口。
///
/// 用的是 `FindWindowW(类名=null, 标题)`,**不限定进程** —— 所以标题必须足够独特。
/// 设置窗口的标题特意带上了应用名(见 `SETTINGS_TITLE`),否则「设置」这种大众词
/// 很容易撞上别的程序里同名的窗口,然后我们就会去改人家的窗口样式。
fn hwnd_by_title(title: &str) -> isize {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: 标题是以 0 结尾的合法宽字符串
        unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) as isize }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        0
    }
}

/// 主窗口句柄。无边框窗口照样有窗口标题,所以还是按标题找。
fn main_hwnd() -> isize {
    hwnd_by_title(WINDOW_TITLE)
}

/// 主显示器的工作区 `(x, y, 宽, 高)` —— 已经排除了任务栏。
///
/// 桌宠默认要停在右下角,也就是贴着工作区的角,而不是屏幕的物理角
/// (否则会被任务栏盖住半截)。
pub fn work_area() -> (i32, i32, i32, i32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};
        let mut r = RECT::default();
        // SAFETY: 传的是本地 RECT 的指针,调用期间一直有效
        unsafe {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut RECT as *mut _, 0);
        }
        (r.left, r.top, r.right - r.left, r.bottom - r.top)
    }
    #[cfg(not(windows))]
    {
        (0, 0, 1920, 1080)
    }
}

/// 鼠标指针在屏幕上的物理坐标。
///
/// 拖桌宠用:窗口是跟着指针走的,**窗口内的相对坐标不会变**(指针和窗口一起移动),
/// 所以算位移只能用这个全局坐标,拿 `mouse-x` 那类窗口内坐标算出来永远是 0。
pub fn cursor_pos() -> (i32, i32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT::default();
        // SAFETY: 传的是本地 POINT 的指针,调用期间一直有效
        unsafe {
            GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }
    #[cfg(not(windows))]
    {
        (0, 0)
    }
}

/// 是不是处于「最大化」状态(这个状态由我们自己维护,见 toggle_maximize)
pub fn is_maximized() -> bool {
    MAXIMIZED.load(std::sync::atomic::Ordering::Relaxed)
}

static MAXIMIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RESTORE_RECT: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

/// 最大化 / 还原,返回切换后的状态。
///
/// 没用 `ShowWindow(SW_MAXIMIZE)`:无边框窗口那样最大化会盖住任务栏。
/// 改成自己按「显示器工作区」摆位置,顺便把还原要用的矩形记下来。
pub fn toggle_maximize() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, SWP_NOZORDER, SPI_GETWORKAREA, SetWindowPos, SystemParametersInfoW,
        };

        let hwnd = main_hwnd();
        if hwnd == 0 {
            return false;
        }
        // SAFETY: 句柄有效;RECT 都是本地初始化过的。
        unsafe {
            if is_maximized() {
                if let Some((x, y, w, h)) = RESTORE_RECT.lock().ok().and_then(|r| *r) {
                    SetWindowPos(hwnd as _, std::ptr::null_mut(), x, y, w, h, SWP_NOZORDER);
                }
                MAXIMIZED.store(false, std::sync::atomic::Ordering::Relaxed);
            } else {
                let mut now = RECT::default();
                GetWindowRect(hwnd as _, &mut now);
                if let Ok(mut slot) = RESTORE_RECT.lock() {
                    *slot = Some((now.left, now.top, now.right - now.left, now.bottom - now.top));
                }
                let mut work = RECT::default();
                SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as *mut _, 0);
                SetWindowPos(
                    hwnd as _,
                    std::ptr::null_mut(),
                    work.left,
                    work.top,
                    work.right - work.left,
                    work.bottom - work.top,
                    SWP_NOZORDER,
                );
                MAXIMIZED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    is_maximized()
}

/// 按「窗口矩形是不是铺满工作区」判断最大化。
///
/// 不靠自己那个标志:用户把窗口拖到屏幕顶部、或者按 Win+Up 也会最大化,
/// 那些是系统干的,我们得看实际状态才知道标题栏按钮该画哪个图标。
pub fn window_fills_work_area() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, SPI_GETWORKAREA, SystemParametersInfoW,
        };
        let hwnd = main_hwnd();
        if hwnd == 0 {
            return false;
        }
        // SAFETY: 句柄有效,两个 RECT 都是本地初始化的
        unsafe {
            let mut now = RECT::default();
            let mut work = RECT::default();
            GetWindowRect(hwnd as _, &mut now);
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as *mut _, 0);
            // 留 2px 容差,免得因为边框差一像素判不出来
            (now.left - work.left).abs() <= 2
                && (now.top - work.top).abs() <= 2
                && (now.right - work.right).abs() <= 2
                && (now.bottom - work.bottom).abs() <= 2
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 给某个窗口设圆角,返回是否设置成功。
///
/// `DWMWA_WINDOW_CORNER_PREFERENCE` 是 Windows 11 才有的窗口属性,旧系统上
/// 会返回失败 HRESULT —— 返回 false,调用方决定要不要记一笔,不影响功能。
fn apply_rounding(hwnd: isize, rounded: bool) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmSetWindowAttribute,
        };
        if hwnd == 0 {
            return false;
        }
        let pref: i32 = if rounded { DWMWCP_ROUND } else { DWMWCP_DONOTROUND };
        // SAFETY: 句柄来自 FindWindowW;pref 是栈上有效、活得比这次调用长的 i32,
        // 大小按字节数如实上报(这个属性要的就是一个 i32)。
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd as _,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            )
        };
        hr >= 0
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, rounded);
        false
    }
}

/// 主窗口的圆角状态,避免每轮轮询都去调一次 DWM。
/// -1 = 还没设过,0 = 已设成直角,1 = 已设成圆角,2 = 系统不支持(别再试)
static MAIN_ROUNDING: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

/// 主窗口四角是否圆角 —— **跟着外观走**:毛玻璃给圆角,原主题给直角
/// (恢复改造前的样子)。半透明面板配直角窗口会很生硬,所以这个差别值得保留。
///
/// 由定时器反复调(要拿到窗口句柄才能设),内部只在值变化时才真的调 DWM。
pub fn set_main_rounding(rounded: bool) {
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering;

        const UNSUPPORTED: i8 = 2;
        let want: i8 = if rounded { 1 } else { 0 };
        let last = MAIN_ROUNDING.load(Ordering::Relaxed);
        if last == want || last == UNSUPPORTED {
            return;
        }
        let hwnd = main_hwnd();
        if hwnd == 0 {
            return; // 窗口还没建出来,下一轮再说
        }
        if apply_rounding(hwnd, rounded) {
            MAIN_ROUNDING.store(want, Ordering::Relaxed);
        } else {
            // Win11 之前必然失败,反复试只会让日志刷屏
            MAIN_ROUNDING.store(UNSUPPORTED, Ordering::Relaxed);
            log("圆角窗口属性不被支持,主窗口保持系统默认直角");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = rounded;
    }
}

/// 设置窗口的圆角。它只在创建/切外观时调,次数少,不用缓存。
pub fn set_settings_rounding(rounded: bool) {
    apply_rounding(hwnd_by_title(SETTINGS_TITLE), rounded);
}

/// 把窗口从任务栏和 Alt+Tab 里拿掉。
///
/// 设置窗口是主窗口的附属面板,单独占一个任务栏格子很碍事。
/// 加的是 `WS_EX_TOOLWINDOW` —— 光靠 SetWindowLongPtr 改扩展样式,任务栏不一定会
/// 立刻重算,所以要跟一个 `SWP_FRAMECHANGED` 让系统重新评估窗口边框。
pub fn hide_from_taskbar(title: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
            SetWindowLongPtrW, SetWindowPos, WS_EX_TOOLWINDOW,
        };
        let hwnd = hwnd_by_title(title);
        if hwnd == 0 {
            return; // 窗口还没建出来
        }
        // SAFETY: 句柄来自 FindWindowW;样式位读改写都是同一线程顺序执行。
        unsafe {
            let ex = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW as isize);
            SetWindowPos(
                hwnd as _,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
    }
}

/// 把某个窗口叫到前台(设置窗口用;主窗口那条见 `focus_existing_window`)。
pub fn bring_to_front(title: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SetForegroundWindow, ShowWindow};
        let hwnd = hwnd_by_title(title);
        if hwnd != 0 {
            // SAFETY: 句柄来自 FindWindowW
            unsafe {
                ShowWindow(hwnd as _, SW_RESTORE);
                SetForegroundWindow(hwnd as _);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
    }
}

/// 窗口是不是被最小化(缩进任务栏)了。
///
/// 用 `IsIconic` **直接问系统**,不用 Slint 的 `Window::is_minimized()` —— 那个值
/// 跟着 winit 事件更新,「刚调完还原、事件还没走完一轮」的时候读到的还是 true。
/// 宿主正是在那种时刻需要马上做判断(见 `State::reveal`)。
pub fn is_minimized(title: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
        let hwnd = hwnd_by_title(title);
        if hwnd == 0 {
            return false; // 窗口还没建出来,谈不上最小化
        }
        // SAFETY: 句柄来自 FindWindowW
        unsafe { IsIconic(hwnd as _) != 0 }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        false
    }
}

/// 把最小化的窗口还原回来。
///
/// **只在真的最小化时才调。** `SW_RESTORE` 对**最大化**的窗口同样是"还原"
/// (MSDN 原话:如果窗口是最小化或最大化,系统都会把它恢复成原大小和位置),
/// 所以不能拿它当"顺便激活一下"用 —— 用户最大化着主窗口,点个红点,
/// 窗口反而被缩回去了。
pub fn restore_window(title: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindow};
        let hwnd = hwnd_by_title(title);
        if hwnd != 0 {
            // SAFETY: 句柄来自 FindWindowW
            unsafe { ShowWindow(hwnd as _, SW_RESTORE) };
        }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
    }
}

/// 把窗口叫到前台,**但不改变它的大小状态**:最大化的保持最大化,普通大小的保持原样。
///
/// 项目里另有一个 `bring_to_front`,它用的是 `SW_RESTORE` —— 那个是给
/// 「自己弹出来的小窗」(对话/笔记/设置)用的,它们无所谓最大化;主窗口不行,
/// 见 `restore_window` 的说明。要还原最小化的窗口请先调 `restore_window`。
pub fn focus_window(title: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
        let hwnd = hwnd_by_title(title);
        if hwnd != 0 {
            // SAFETY: 句柄来自 FindWindowW
            unsafe { SetForegroundWindow(hwnd as _) };
        }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
    }
}

pub fn minimize_window() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_MINIMIZE, ShowWindow};
        let hwnd = main_hwnd();
        if hwnd != 0 {
            // SAFETY: 句柄来自 FindWindowW
            unsafe { ShowWindow(hwnd as _, SW_MINIMIZE) };
        }
    }
}

// ── 系统通知 ──────────────────────────────────────────────────────────

/// 弹一条 Windows 通知。
///
/// 用 PowerShell 的 AppID:它一定注册过,通知能稳定显示。代价是通知来源显示成
/// 「Windows PowerShell」而不是本应用 —— 要改成自己的名字,得在装包时创建带
/// AUMID 的开始菜单快捷方式(见 docs/calendar-reminders.md §8.2)。
pub fn notify(title: &str, body: &str) {
    #[cfg(windows)]
    {
        use tauri_winrt_notification::Toast;
        if let Err(err) = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(title)
            .text1(body)
            .show()
        {
            log(&format!("弹通知失败: {err}"));
        }
    }
    #[cfg(not(windows))]
    {
        log(&format!("[提醒] {title}: {body}"));
    }
}

// ── 开机自启 ──────────────────────────────────────────────────────────

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .and_then(|key| key.get_value::<String, _>(AUTOSTART_VALUE))
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 开关开机自启。写的是 HKCU,不需要管理员权限。
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
            .map_err(|e| format!("打开注册表失败: {e}"))?;

        if enabled {
            let exe = std::env::current_exe().map_err(|e| format!("找不到程序路径: {e}"))?;
            // 路径带空格时一定要加引号,否则开机时会启动失败
            let command = format!("\"{}\"", exe.display());
            key.set_value(AUTOSTART_VALUE, &command)
                .map_err(|e| format!("写入自启项失败: {e}"))?;
        } else {
            // 本来就没有的话,删不掉也不算错
            match key.delete_value(AUTOSTART_VALUE) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("删除自启项失败: {err}")),
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("当前平台不支持开机自启".to_string())
    }
}

// ── 托盘 ──────────────────────────────────────────────────────────────

/// 用户在托盘菜单里点了什么
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayAction {
    Open,
    Today,
    Settings,
    /// 勾选 / 取消桌宠
    TogglePet(bool),
    Quit,
}

/// 托盘图标。**必须一直持有**,drop 掉图标就没了。
pub struct Tray {
    _icon: tray_icon::TrayIcon,
    open_id: tray_icon::menu::MenuId,
    today_id: tray_icon::menu::MenuId,
    settings_id: tray_icon::menu::MenuId,
    pet_item: tray_icon::menu::CheckMenuItem,
    quit_id: tray_icon::menu::MenuId,
}

impl Tray {
    pub fn new(show_pet: bool) -> Result<Self, String> {
        use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
        use tray_icon::{Icon, TrayIconBuilder};

        let open = MenuItem::new("打开主窗口", true, None);
        let today = MenuItem::new("今天", true, None);
        let settings = MenuItem::new("设置", true, None);
        // 可勾选:桌宠不是所有人都想要
        let pet = CheckMenuItem::new("显示桌宠", true, show_pet, None);
        let quit = MenuItem::new("退出", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &today,
            &settings,
            &pet,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| format!("建托盘菜单失败: {e}"))?;

        let icon = Icon::from_rgba(app_icon_rgba(), ICON_SIZE, ICON_SIZE)
            .map_err(|e| format!("建托盘图标失败: {e}"))?;

        let tray = TrayIconBuilder::new()
            .with_tooltip("待办清单")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build()
            .map_err(|e| format!("创建托盘失败: {e}"))?;

        Ok(Self {
            _icon: tray,
            open_id: open.id().clone(),
            today_id: today.id().clone(),
            settings_id: settings.id().clone(),
            pet_item: pet,
            quit_id: quit.id().clone(),
        })
    }

    /// 非阻塞地取一个用户动作。由 main.rs 的定时器轮询。
    pub fn poll(&self) -> Option<TrayAction> {
        use tray_icon::menu::MenuEvent;

        // 菜单事件
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.open_id {
                return Some(TrayAction::Open);
            }
            if event.id == self.today_id {
                return Some(TrayAction::Today);
            }
            if event.id == self.settings_id {
                return Some(TrayAction::Settings);
            }
            if event.id == self.pet_item.id() {
                return Some(TrayAction::TogglePet(self.pet_item.is_checked()));
            }
            if event.id == self.quit_id {
                return Some(TrayAction::Quit);
            }
        }
        // 左键单击图标也当「打开主窗口」
        while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                return Some(TrayAction::Open);
            }
        }
        None
    }
}

const ICON_SIZE: u32 = 32;

/// 现画一个 32x32 的托盘图标:圆角方块 + 白色对勾。
/// 不从文件读是为了不在仓库里塞二进制资源,也省得管 .ico 的构建流程。
fn app_icon_rgba() -> Vec<u8> {
    const ACCENT: [u8; 3] = [0x3b, 0x74, 0xf2];
    const MARK: [u8; 3] = [0xff, 0xff, 0xff];
    const R: f32 = 7.0; // 圆角半径
    let n = ICON_SIZE as f32;
    let mut px = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);

    // 对勾的两段线:(x, y) 归一化到 0..1
    let seg = |p: (f32, f32), a: (f32, f32), b: (f32, f32)| -> f32 {
        let (px_, py) = p;
        let (ax, ay) = a;
        let (bx, by) = b;
        let dx = bx - ax;
        let dy = by - ay;
        let len2 = dx * dx + dy * dy;
        let t = (((px_ - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
        let (cx, cy) = (ax + t * dx, ay + t * dy);
        ((px_ - cx).powi(2) + (py - cy).powi(2)).sqrt()
    };

    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // 圆角矩形内部判定
            let cx = fx.clamp(R, n - R);
            let cy = fy.clamp(R, n - R);
            let inside = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= R;

            let p = (fx / n, fy / n);
            let on_mark = seg(p, (0.28, 0.52), (0.44, 0.68)) < 0.055
                || seg(p, (0.44, 0.68), (0.73, 0.34)) < 0.055;

            let (rgb, alpha) = if !inside {
                ([0u8; 3], 0u8)
            } else if on_mark {
                (MARK, 255)
            } else {
                (ACCENT, 255)
            };
            px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], alpha]);
        }
    }
    px
}
