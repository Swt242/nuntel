# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目

Windows 桌面小挂件(**Nuntel**):桌宠停在桌面角落,待办清单、Markdown 笔记、
AI 对话都从它展开。单 crate(`nuntel`),Rust 业务逻辑 + Slint 界面。

⚠️ **应用名叫 Nuntel,但各个窗口的标题里没有它** —— 窗口标题只写「自己是干什么的」
(「待办清单」「待办清单 · 笔记」…)。这几个标题是拿来 `FindWindowW` 的,
`platform.rs` 里那几个常量改名时**不要**顺手加应用名。应用名只出现在:
exe 文件名、托盘悬停提示、数据目录。

**代码注释、UI 文案、文档、提交信息全是中文** —— 新增内容请跟着写中文。注释密度偏高,
且习惯写「为什么这么做」和「踩过什么坑」,`docs/calendar-reminders.md` 里带 § 编号
(代码注释直接引用 `见 §23.1`),动手改之前先扫一眼相关的 §。

## 常用命令

```sh
cargo run --release --features skia   # 推荐:文字最锐 + 桌宠能透明
cargo run                             # 不带 skia:文字稍糊,桌宠是一块黑方块
cargo test --release                  # 79 个单测,应该全过
cargo test --release fling            # 只跑名字含 "fling" 的
cargo test --release model::tests::badge_shows_overdue_and_relative_days
```

- **默认 MSVC 工具链**(本机装了 VS 2022 Build Tools)。`run.sh` 是没装 MSVC 时的
  GNU 后备方案,会补 MinGW 导入库和 binutils,用 `./run.sh test --release` 这样调。
  ⚠️ `--features skia` 在 GNU 工具链下**编不过**(Skia 只有 MSVC 的预编译包)。
- **release 没有控制台**(源码首行 `windows_subsystem = "windows"`),所有诊断走
  `platform::log` 写进 `%APPDATA%\nuntel\nuntel.log`(启动时超过 256KB 截断)。
  调试时报错看不到就去读这个文件。debug 构建才有控制台。
- **进程是单实例的**(命名互斥体):已经开着一个时再启动只会把原窗口叫到前台然后退出。
  所以要重建 exe 得先杀掉正在跑的那个,否则链接会因 exe 被占用失败。
- 只想改样式不用编 Rust:`~/.slint-tools/slint-viewer.exe --check ui/app.slint`
  (`--load-data` 认不出跨文件 import 的 global,预览整窗口请直接跑程序)。
- 让 AI 驱动运行中的界面做回归:
  `SLINT_EMIT_DEBUG_INFO=1 cargo build --features slint/mcp`,
  再 `SLINT_MCP_PORT=9315 ./target/release/nuntel.exe`,连 `http://localhost:9315/mcp`。
  界面里关键控件都起了 id(`AppWindow::input`、`TodoRow::clock`、`CalendarView::goto-today`)。

## 架构

### 状态在 Rust,UI 只渲染

`ui/widgets.slint` 里三个 `export global` 是两边的全部契约:

| 单例 | 方向 | 内容 |
|---|---|---|
| `AppData` | Rust → UI | 清单模型、日历 42 格、选中日、计数、时间编辑态、`pet-*` 几何、`chat-*`、`md-*` |
| `Logic` | UI → Rust | 一堆 `callback`:添加/勾选/删除/翻月/设时间/拖拽/聊天/保存笔记… |
| `Theme` | Rust 只写 `preference` + `appearance` | 其余 ~40 个颜色令牌和 `radius-scale` 都在 Slint 里派生 |

Rust 侧 `Vec<Todo>` 是唯一数据源,**每次变更都重建一份按当前过滤条件筛好的模型**推给 UI;
任务带稳定 `id`,回调传 id 不传下标(过滤/换视图后下标会变)。所有业务逻辑在 `main.rs` 的
`impl State` 里(Slint 侧不持有任何业务数据)。

### ⚠️ 全局单例不跨窗口共享

Slint 文档原话:*Global singletons aren't shared between separate windows.* 每个窗口
(`AppWindow` / `SettingsWindow` / `PetWindow` / `ChatWindow` / `NotesWindow`)各持一份
`Theme` / `AppData` / `Logic`。**只改主窗口那份,别的窗口纹丝不动。**

所以有个 `State::push_theme()`,凡是动主题的地方都走它,五个窗口一起推。
**加新窗口时这里最容易漏** —— 漏了的表现是那个窗口一直用默认的「跟随系统」亮色。

同理,每个窗口的回调都要单独注册(`pet.global::<Logic>().on_pet_clicked(...)`),
而且只能在 `main()` 里注册(闭包要捕获 `Rc<State>`,那时才存在)。

### 窗口与显示

五个窗口**启动时就全部建好并隐藏**(不是打开时才建 —— 避免闪烁和位置丢失),都存成
`Rc<Window>`(生成的类型不是 `Clone`)。**启动只显示桌宠**,主窗口故意不上屏;其余界面
从桌宠的悬停环或托盘菜单打开。唯一的退出路径是托盘「退出」。

两个必须成对出现的辅助函数:

- `State::reveal(ui, title)`(`src/main.rs:2748`):`show()` 之后调 `nudge_window`。
  因为用带 alpha 的软件渲染器时,Slint 在窗口映射前先渲染的那一帧会让 softbuffer
  认定「缓冲区里已经有内容」,系统给的新画面是全透明的,而 Slint 只补画脏块 ——
  结果**窗口只有一两小块有内容,其余透出桌面**。
- `State::nudge_window`(`src/main.rs:2788`):高度顶 1px,120ms 后收回。尺寸一变
  softbuffer 就重分配缓冲区、age 归 0,Slint 才会整窗口重画。从外面
  `request_redraw()` 没用(那时 winit 窗口还没建出来);
  两次改尺寸挤在同一个 tick 里也会被合成一次、等于没改。
  另外 `Window::size()` 在**最小化**时返回的是任务栏缩略图尺寸(160x28),
  所以先问 `platform::is_minimized`(实时 `IsIconic`,不是 Slint 那个滞后的缓存值)。

### 事件循环

`slint::run_event_loop_until_quit()`(`src/main.rs:3559`)—— 故意不用 `ui.run()`,
后者在没有可见窗口时就会退出,而托盘是 `tray-icon` crate 的对象、Slint 并不知道它。

没有 async runtime,全靠 Slint `Timer` + 两个 `std::sync::mpsc` channel:

| 定时器 | 间隔 | 干什么 |
|---|---|---|
| `tray_timer` | 300ms | 排空托盘事件、`poll_window`(紧凑/最大化/设置窗的位置修正) |
| `pet_timer` | 40ms | 桌宠逐帧推进 + **悬停环判定**(读全局光标) |
| `reminder_timer` | 20s | 提醒扫描 |
| `fling_timer` | 16ms | 滚轮惯性,只在滑行期间开 |
| `chat_timer` | 50ms | 排空后台线程的流式增量,只在请求期间开 |
| `notes_timer` | 120ms | 草稿本预览 |

Slint 的 `Timer` **被 drop 就停**,所以必须存进 `State`(别用临时变量)。

## 约定

### 几何与主题

- **窗口大小/元素坐标这类几何,统统在 Rust 里算好推进 UI**,Slint 只乘 `1px` 消费
  (`week-card-height`、`pet-ring-radius` 都是范例)。窗口尺寸就是按这几个数定的,
  两边各算一份迟早对不上。
- **窗口根上声明的属性,子元素要用 `root.` 引,`parent.` 不行**(`parent.` 只拿得到内置的
  width/height/background 那些)。笔记窗那几个宽度属性就踩过:子面板写 `parent.list-w`
  编译报 `Element 'Rectangle' does not have a property 'list-w'`。
- **Slint 没有「全局圆角」**,所有 `border-radius` 都写成 `N px * Theme.radius-scale`,
  工业风把它压成 0 = 全直角。新加圆角记得乘上(唯一例外是逾期那种 5px 小圆点)。
- 装饰性令牌(`panel-edge` / `shadow` / `blob-*` / `grid-line`)在轮不到自己的外观下
  直接给 `transparent`,这样 `PanelEdge` 和几十处 `drop-shadow-*` **不用加任何 `if`**
  就自动失效。新加装饰元素可以用这招,别到处塞条件。
- 强调色拆成 `accent`(填充)/ `accent-ink`(文字、图标、描边)两个角色 ——
  工业风的信号黄当填充好用、当文字对比度只有 1.7:1,亮色下 `accent-ink` 取深金。
- **Slint 语言里没有任何模糊原语**,玻璃感靠「本来就是糊的底」(径向渐变色斑)
  和半透明面板 + 1px 发丝边 + 顶部高光(`PanelEdge`)。
- 压在桌面/其它窗口上的浮层用 `Theme.sheet`(几乎不透明),不能用透明底 ——
  别人窗口的颜色会穿过来和自己的字叠在一起。

### 桌宠

- 桌宠是**应用的入口**:透明、置顶、无边框,内容贴底(`VerticalLayout alignment: end`),
  窗口变高时只往上长,**宠物的脚不会跳**。高度 = 宠物 + 角标余量 + 环 + 气泡 + 输入条。
- 悬停那一圈图标只做**上半圈**(桌宠默认在屏幕右下角,下半圈会伸出屏幕/进任务栏);
  几何常量(`RING_ICON` / `RING_GAP` / `RING_SPREAD`)在 `src/main.rs` 顶部,
  改完窗口大小、图标位置、悬停判定范围一起跟着变。
  判定用**宿主每 40ms 轮询全局光标** + 迟滞(进圈看宠物本体、留在圈里看整圈),
  不用 Slint 的 `has-hover` —— 鼠标穿过的空隙会让它闪断。
- 拖动**不能用 `WindowMoveArea`**:它靠 `input_event_filter_before_children` 拿按下,
  之后的 Moved 只发给「按下时抓住鼠标的那个元素」及其祖先,而宠物身上必须有 TouchArea
  接单击/双击,按下会被它抓走,`start_window_move()` 永远不触发。改由宿主按指针的
  **全局**位移挪窗口(窗口跟着指针走,窗口内坐标在拖动中几乎不变)。
- 同层元素**靠后的压在上面、也先拿到鼠标事件**。角标、那一圈图标都必须排在
  `pet-touch` / `pet-body` 后面,否则会被盖住点不到。
- `TouchArea.mouse-x/y` 是**相对本元素**的,算全局位置要用 `self.absolute-position`
  而不是父元素的(`src/main.rs` 的坑记录见 §27.3)。
- 桌宠素材版权归**鹰角网络**,仅供个人使用,不随本项目代码许可授权。
  换素材:逐帧 PNG 丢进 `assets/pet/<名字>/` + 在 `manifest.json` 加一行,**不用改 Rust**
  (帧表由 `build.rs` 生成,用 `include_bytes!` 嵌进 exe,所以与工作目录无关)。
  删掉 `assets/pet/` 桌宠会自动不启动。

### 渲染器

`--features skia` 编出来的版本在 `main()` 里把 `SLINT_BACKEND` 设成
**`winit-skia-software`**(外部已设置则尊重外部值)。

选软件后端是因为**桌宠要透明**:透明要求渲染面带 alpha 通道,而 Slint 的 GL surface
根本不申请,femtovg / winit-skia 会把桌宠画成黑方块。选 skia 的软件后端而不是纯
`winit-software`,是因为它仍然走 Skia 光栅化,文字和 GL 版一样锐
(实测边缘强度 4.70 vs femtovg 3.46)。

## 持久化

数据目录 `%APPDATA%\nuntel\`(`model::data_file()` 一处定义,其余文件由它派生):

| 文件 | 内容 |
|---|---|
| `todos.json` | 待办 + 主题/外观/桌宠开关与尺寸与各动画速度,`version: 2` |
| `notes\*.md` | Markdown 笔记库,一个文件一篇(老的单文件 `notes.md` 会在第一次打开笔记时自动搬进去) |
| `ai.json` | `{ base_url, api_key, model }` —— **故意和 todos 分开存**,把 todos 发给别人时不会顺手带上密钥(明文存,没走凭据管理器) |
| `nuntel.log` | 运行日志 |

- 写盘一律**先写 `.tmp` 再 rename**,避免写一半崩掉毁数据。
- 时间按**本地时区**存成 `"YYYY-MM-DD"` / `"HH:MM"` 字符串,不存 UTC 时间戳 ——
  时区/夏令时变化不会把「9:30 的会」变成 10:30(代价是不支持跨时区)。
- 旧版本数据能直接打开,`version < 2` 时自动备份一份 `todos.json.v1.bak`。
- 提醒的 `notified` 标记会落盘,重启后不会重复提醒。

### 再改名时要动的地方

应用改过一次名(`rgui-todo` → `nuntel`),留下的那套搬迁机制是给下一次用的。
**只改显示用的字符串是不够的** —— 以下每一处漏掉都会有具体症状:

| 改哪 | 漏了会怎样 |
|---|---|
| `Cargo.toml` 的 `name` | exe 还是老名字;`tools/*.ps1` 全都按进程名找窗口,一起失效 |
| `model.rs` 的 `APP_DIR`,并把老名字加进 `OLD_APP_DIRS` | **用户的待办和笔记「全部消失」**(东西还在老目录里躺着) |
| `platform.rs` 的 `AUTOSTART_VALUE`,并把老名字加进 `OLD_AUTOSTART_VALUES` | 开机自启静默失效(注册表里那条还指着已经不存在的老 exe) |
| `tools/*.ps1` 里的 `-Process` 默认值 | 验证脚本全部找不到窗口 |
| 日志文件名 | 靠 `APP_DIR` 派生,跟着一起变;搬迁时会把老的改好名 |

两条搬迁(`model::migrate_old_data_dirs` / `platform::migrate_autostart`)都必须在
**`main()` 的第一件事**执行,连 `platform::log` 都不能先写 —— 日志就在数据目录里,
先写一步就会在新目录建出文件,`new.exists()` 为真、搬迁直接放弃。

**不要动 `platform.rs` 里那几个窗口标题常量**:它们只是「窗口叫什么」,
刻意不带应用名(详见上面「项目」一节),前缀是为了避免和别的程序的同名窗口撞车。

## 模块职责

纯逻辑都拆成了可单测的模块,`main.rs` 只管装配:

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 状态(`State`)、回调接线、定时器、窗口几何。**唯一没有单测的大文件** |
| `src/platform.rs` | Windows 杂活:单实例、托盘、通知、开机自启、无边框窗口控制、DWM 圆角、光标位置;非 Windows 有 no-op 兜底 |
| `src/model.rs` | 数据结构、时间推导、读写盘、v1→v2 迁移、文件路径 |
| `src/reminder.rs` | 提醒判定与通知文案(算出来,不发通知),错过 7 天以上只标记不弹 |
| `src/ai.rs` | OpenAI 兼容协议:拼请求、解析 SSE。**纯协议,不联网不碰 UI** |
| `src/markdown.rs` | Markdown **块级**切分(标题/代码块/引用/分隔线/表格),行内样式故意交给 Slint 的 `StyledText` |
| `src/notes.rs` | 笔记库:`notes\` 下的增删改查、文件名净化、撞名退避、老单文件迁移。**所有函数都显式吃一个 `dir`**,单测才能往临时目录里造文件而不碰用户的库 |
| `src/pet.rs` | 桌宠帧的加载与推进 |
| `src/calendar_info.rs` | 农历、节日、放假调休(离线) |

⚠️ `chinese_holiday` 对 2004-01-01 之前或 2026-12-25 之后的日期会 **assert 直接崩**,
`calendar_info.rs` 里先判范围、范围外退化成「周六周日休息」。库升级后记得改那个范围。

## 测试

全部是模块内联的 `#[cfg(test)] mod tests`,没有 `tests/` 目录。67 个,分布在
`ai`(10)、`reminder`(10)、`markdown`(16)、`calendar_info`(5)、`model`(5)、
`pet`(4)、`main.rs`(17:`mod tests` 时间选择器 6 个 + `mod fling_tests` 滚轮惯性 11 个)。

写测试时的惯例:**纯计算拆成自由函数或独立模块**,这样不用起 UI 就能测。
`main.rs` 里几个可测的辅助函数(`due_from_picker`、`fling_*`)就是为此留在模块级的。

> 单测覆盖不到的是 UI 装配 —— 几轮下来的真 bug(滚轮滑不动、拖拽反向、
> 窗口只画一半)全是靠下面的脚本逮到的。改了 UI 请照着验一遍。

## 验证 UI 的自动化工具

`tools/` 下是一堆验证脚本(排查 UI 问题留下的家当,`.gitignore` 不收 `tmp/` 里的产物)。
它们用 `FindWindowW` + `SetCursorPos` + `mouse_event` 驱动真实窗口:

- `tools/pet-ring.py`(Python)—— **首选范式**:一个进程里一口气做完「挪光标 → 等环弹出 →
  PrintWindow 截图 → 按几何算出图标坐标 → 合成点击 → 报哪个窗口变可见」。
  为什么必须一口气做完:悬停判定读的是**全局光标**,这台机器上真鼠标随时会被动一下,
  分几步做中间光标就跑掉了,环会自己收起来,看起来像「根本没弹」。
- `tools/badge-click.py` —— 被 `pet-ring.py` import,提供 `find` / `rect_of` / `click_at` /
  `grab` / `BITMAPINFO` 这些底座。
- `tools/shotwin.py` —— 按标题截任意窗口(走 `PrintWindow`,不怕被别的窗口盖住)。
- `tools/screenshot.ps1` —— 按窗口矩形精确截主窗口存 PNG(README 里那几张就是它截的)。
- `tools/mock-openai.py` —— 本地假 OpenAI 服务,不花 API 费用就能验流式链路,
  而且**故意把 SSE 切成 3 字节一片**,专测分块不对齐。

几条踩出来的规矩:

- **截图用 `PrintWindow(PW_RENDERFULLCONTENT)`,别用屏幕抓取。** Windows 通知横幅也是
  置顶层,能压在置顶的桌宠上面,抓屏会得到一张「宠物被盖住」的假象。
  代价是没画到的地方 alpha=0,拼出来是黑的,合成到中灰上才看得清。
- **`PrintWindow` 读不出透明窗口的 alpha** —— 桌宠四角永远返回
  `alpha=255, BGRA=(0,0,0)`。所以「这个窗口到底透不透明」**不能用 PrintWindow 判断**,
  它只会给你一张不透明的图;要判断得**抓屏**看桌面有没有从头像周围透出来。
- ctypes 调 `SetWindowPos` 必须显式声明 `argtypes`,否则 `HWND_TOPMOST`(-1) 以 32 位传递、
  调用静默失败(返回 0)。
- 脚本里的几何常量(`tools/pet-ring.py` 顶部那三个)**必须和 `src/main.rs` 里的保持一致**,
  宿主一改脚本也得改。
- 验证完**把数据和窗口位置恢复原样**,别在用户数据里留下测试痕迹。
- **`mcp__windows-control__click` 能点,但不动真光标** —— 凡是「悬停才出现」的东西
  (笔记行的删除按钮之类)用它验不了,截图里那个按钮永远不出现,看着像功能坏了。
  这一路要用 `SetCursorPos` + `mouse_event`。
- **合成的双击打不开对话窗**:`mouse_event` 连点两下,`pet-clicked` 每次都进、
  `double-clicked` 一次都不进(试过 20~350ms 五档间隔)。怀疑是非激活工具窗口
  收不到 `WM_LBUTTONDBLCLK`。**对话窗目前只有双击桌宠一个入口**,所以那条路上的
  东西自动化验不到,见 §29.6。

## 文档

- `README.md`(457 行)—— 给用户看的:功能、外观主题、桌宠、AI 对话、Markdown、
  渲染器选择、代码结构。改动面向用户的特性时同步更新。
- `docs/calendar-reminders.md`(1946 行)—— 产品设计文档 + 每轮的验收结果,
  按 § 编号,§11 之后每一节末尾都有「这一轮踩到的坑」。**代码注释直接引用这些编号**,
  所以新增一节时接着编号往下写,别插队。
