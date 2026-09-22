//! 笔记库 —— `%APPDATA%\nuntel\notes\` 下的多个 Markdown 文件。
//!
//! 原来草稿本是**单文件**(`notes.md`,见 `model::notes_file` 的说明)。用户要
//! 「支持文件系统」,于是改成一个小库:**这一层目录下的 `*.md` 就是全部笔记**,
//! 不递归、不做子目录、不做文件夹选择器。文件名(去掉 `.md`)既是显示名也是 id。
//!
//! 为什么单拆一个模块:净化文件名、撞名退避、老文件迁移这些全是「纯逻辑 + 文件操作」,
//! 不碰界面就能单测 —— 和 `ai.rs` / `markdown.rs` / `reminder.rs` 一个路子。
//! 宿主(`main.rs`)只负责把这儿的结果搬进界面。
//!
//! ⚠️ **每个函数都显式吃一个 `dir`**,不自己去问 `notes_dir()`:单测要往临时目录里
//! 造文件,不能碰用户真实的笔记库。`model.rs` 的 `load(path)` / `save(path, ..)`
//! 也是这么分开的。

use std::path::{Path, PathBuf};

/// 库目录:`%APPDATA%\nuntel\notes\`,和 `todos.json`、`ai.json` 同一个目录下。
pub fn notes_dir() -> PathBuf {
    crate::model::data_file().with_file_name("notes")
}

/// 新建笔记的默认名。撞名了往后加序号(见 [`unique_name`])。
const DEFAULT_NAME: &str = "新建笔记";

/// 老的单文件 `notes.md` 迁进库时用的名字。
const LEGACY_NAME: &str = "笔记";

/// 名字最长多少个字符(不含扩展名)。列表里一行放不下那么长的,而且 Windows
/// 整条路径本来就有限制 —— 80 足够写清楚「这是什么内容」了。
const MAX_NAME: usize = 80;

// ── 名字的净化 ────────────────────────────────────────────────────────

/// 把用户输入的名字收拾成能当文件名用。
///
/// 挡掉的是**会出事**的那几类,不是「不合法字符」这么笼统:
/// - 路径分隔符(`/` `\`)和 `.` / `..` —— 不挡的话 `../../todos.json`
///   这种名字能一路写出库目录去,把别的数据文件覆盖掉;
/// - Windows 保留字符(`: * ? " < > |`)—— 写盘会直接失败;
/// - 控制字符 —— 名字里混进 `\n` 之后日志和列表都会错位;
/// - 保留设备名(`CON` `NUL` `COM1` …)—— 这是 Windows 上一个古老但真实的坑,
///   叫 `NUL.md` 的文件**删不掉也读不出**;
/// - 结尾的点和空格 —— Windows 存盘时会悄悄吃掉,于是「存进去」和「列出来」
///   得到的名字不一样,列表里会出现两个看着相同的项。
///
/// 无害的白名单不动:中文、空格、括号、`-` 都留着。`None` = 这个名字不能用。
pub fn sanitize(raw: &str) -> Option<String> {
    let mut name = raw.trim().to_string();

    // 用户可能连后缀一起打进来(「笔记.md」),去掉 —— 扩展名由库自己管。
    //
    // ⚠️ 先 `to_ascii_lowercase()` 再 `ends_with`,**不能**直接切
    // `name[name.len() - 3..]`:名字可能是中文,`len()` 是字节数,
    // 减 3 未必落在字符边界上,切下去直接 panic。
    // `to_ascii_lowercase` 只动 ASCII 字节,长度和边界都不变,所以能这么比。
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") && name.len() > 3 {
        name.truncate(name.len() - 3);
    }

    // 结尾的点和空格:Windows 存盘时会吃掉,导致读写名字对不上
    name = name.trim_end_matches([' ', '.']).trim().to_string();

    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    // 点开头的在列表里是「隐藏文件」,存进去就再也看不见了 —— 直接不收
    if name.starts_with('.') {
        return None;
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return None;
    }
    if is_reserved_device_name(&name) {
        return None;
    }
    if name.chars().count() > MAX_NAME {
        return None;
    }
    Some(name)
}

/// `CON` / `PRN` / `AUX` / `NUL` / `COM1`..`COM9` / `LPT1`..`LPT9`(大小写不敏感,
/// 且**带不带扩展名都一样**:Windows 把 `NUL.md` 也当设备)。
fn is_reserved_device_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // 全程按**字节**比,不切片成 &str:名字里可能有中文甚至 emoji,
    // 按字节数是 4 但按字符切会落在字符中间,`&s[..3]` 那种写法会 panic。
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (bytes.starts_with(b"COM") || bytes.starts_with(b"LPT"))
        && (b'1'..=b'9').contains(&bytes[3])
}

/// 给库目录里起一个不撞的名字:`新建笔记`、`新建笔记 2`、`新建笔记 3`…
///
/// 序号从 2 开始 —— 「新建笔记 1」看着像谁手工编的号,2 才读得出「这是第二个」。
pub fn unique_name(existing: &[String]) -> String {
    if !existing.iter().any(|n| n == DEFAULT_NAME) {
        return DEFAULT_NAME.to_string();
    }
    (2..)
        .map(|i| format!("{DEFAULT_NAME} {i}"))
        .find(|candidate| !existing.iter().any(|n| n == candidate))
        .expect("序号总能找到一个不撞的")
}

// ── 列表 ──────────────────────────────────────────────────────────────

/// 库里的全部笔记名(不含 `.md`),按名字排序。
///
/// 只认这一层、只认 `*.md`、跳过目录和点开头的隐藏文件。目录不存在就返回空 ——
/// 还没建过库是很正常的状态,不该报错。
pub fn list(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let file_name = e.file_name().to_string_lossy().into_owned();
            if file_name.starts_with('.') {
                return None;
            }
            let stem = file_name.strip_suffix(".md").or_else(|| {
                // 大小写不敏感地再试一次(`.MD` 也该认)
                let lower = file_name.to_ascii_lowercase();
                lower.ends_with(".md").then(|| &file_name[..file_name.len() - 3])
            })?;
            (!stem.is_empty()).then(|| stem.to_string())
        })
        .collect();
    // 大写小写混着时按小写比,不然 "Zebra" 会排到 "apple" 前面
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// 名字 → 库里的完整路径。名字不合法给 `None`。
pub fn path_in(dir: &Path, name: &str) -> Option<PathBuf> {
    sanitize(name).map(|clean| dir.join(clean).with_extension("md"))
}

/// 「存到笔记」在笔记窗口没开着的时候用:挑最近改过的那篇。
pub fn most_recent(dir: &Path) -> Option<String> {
    list(dir)
        .into_iter()
        .filter_map(|name| {
            let path = path_in(dir, &name)?;
            let time = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((time, name))
        })
        .max_by_key(|(time, _)| *time)
        .map(|(_, name)| name)
}

// ── 增删改 ────────────────────────────────────────────────────────────

/// 新建一篇空笔记,返回它的名字。
pub fn create(dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建笔记目录失败: {e}"))?;
    let name = unique_name(&list(dir));
    let path = path_in(dir, &name).ok_or_else(|| "名字不可用".to_string())?;
    std::fs::write(&path, "").map_err(|e| format!("新建笔记失败: {e}"))?;
    Ok(name)
}

/// 改名。返回收拾干净之后真正用上的名字(和传进来的可能不一样)。
pub fn rename(dir: &Path, old: &str, new: &str) -> Result<String, String> {
    let clean = sanitize(new).ok_or_else(|| "这个名字不能用".to_string())?;
    let from = path_in(dir, old).ok_or_else(|| "这一篇找不到了".to_string())?;
    if clean == old {
        return Ok(clean); // 名字没变,别白动一次文件
    }
    let to = path_in(dir, &clean).ok_or_else(|| "这个名字不能用".to_string())?;
    if to.exists() {
        return Err(format!("已经有一篇叫「{clean}」的笔记了"));
    }
    std::fs::rename(&from, &to).map_err(|e| format!("改名失败: {e}"))?;
    Ok(clean)
}

/// 删掉一篇。**直接删文件,不进回收站** —— 界面上那个删除按钮因此要点两次。
pub fn remove(dir: &Path, name: &str) -> Result<(), String> {
    let path = path_in(dir, name).ok_or_else(|| "这一篇找不到了".to_string())?;
    std::fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))
}

/// 读一篇的内容。读不出来(删了、编码坏了)就当空的,别让窗口打不开。
pub fn read(dir: &Path, name: &str) -> String {
    path_in(dir, name)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

/// 写一篇。目录不存在就现建。
pub fn write(dir: &Path, name: &str, text: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建笔记目录失败: {e}"))?;
    let path = path_in(dir, name).ok_or_else(|| "这一篇找不到了".to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("保存失败: {e}"))
}

// ── 老数据迁移 ────────────────────────────────────────────────────────

/// 把老的单文件 `notes.md` 搬进库里,叫「笔记」。
///
/// **用 `rename` 而不是复制**:同一个目录下是原子的,而且搬完老文件就没了 ——
/// 这个函数因此天然只生效一次,不需要额外的「迁移过了」标记。
/// 失败就当作没迁过,下次启动再试,不会丢内容。
///
/// 之所以不等「库目录不存在」才迁:用户可能把库里的笔记全删了,那时目录还在、
/// 而老文件还在 —— 按目录判断会把这个文件永久晾在外面。
pub fn migrate_legacy(dir: &Path) {
    let legacy = crate::model::notes_file();
    if !legacy.exists() {
        return;
    }
    if let Err(err) = std::fs::create_dir_all(dir) {
        crate::platform::log(&format!("迁移老笔记:建目录失败,这次跳过: {err}"));
        return;
    }
    // 目标名撞了就退避(库里可能已经手工建过一篇叫「笔记」的)
    let mut name = LEGACY_NAME.to_string();
    let existing = list(dir);
    for i in 2.. {
        if !existing.iter().any(|n| n == &name) {
            break;
        }
        name = format!("{LEGACY_NAME} {i}");
    }

    match path_in(dir, &name) {
        Some(to) => match std::fs::rename(&legacy, &to) {
            Ok(()) => crate::platform::log(&format!(
                "老的 {} 已迁进笔记库,现在是 {}",
                legacy.display(),
                to.display()
            )),
            Err(err) => crate::platform::log(&format!("迁移老笔记失败,这次跳过: {err}")),
        },
        None => crate::platform::log("迁移老笔记:目标名不可用,这次跳过"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个临时目录。**不碰用户真实的笔记库** —— 所有函数都吃显式 `dir` 就是为了这个。
    ///
    /// 名字里带上测试名和进程号:测试是并行跑的,固定名字会互相踩。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rgui-notes-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_keeps_harmless_names() {
        assert_eq!(sanitize("会议记录").as_deref(), Some("会议记录"));
        assert_eq!(sanitize("  笔记 2026  ").as_deref(), Some("笔记 2026"));
        assert_eq!(sanitize("a-b(c)").as_deref(), Some("a-b(c)"));
    }

    #[test]
    fn sanitize_strips_md_suffix() {
        // 用户连后缀一起打进来是常事
        assert_eq!(sanitize("笔记.md").as_deref(), Some("笔记"));
        assert_eq!(sanitize("笔记.MD").as_deref(), Some("笔记"));
        // 但只有一个「.md」的输入不该被清成空
        assert_eq!(sanitize("md").as_deref(), Some("md"));
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        // 这几条是关键:放过去就能写出库目录、覆盖 todos.json
        assert_eq!(sanitize("../todos"), None);
        assert_eq!(sanitize("a/b"), None);
        assert_eq!(sanitize("a\\b"), None);
        assert_eq!(sanitize(".."), None);
        assert_eq!(sanitize("."), None);
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("   "), None);
    }

    #[test]
    fn sanitize_rejects_windows_landmines() {
        assert_eq!(sanitize("a:b"), None);
        assert_eq!(sanitize("a*b"), None);
        assert_eq!(sanitize("a?b"), None);
        assert_eq!(sanitize("a\"b"), None);
        assert_eq!(sanitize("a<b>c"), None);
        assert_eq!(sanitize("a|b"), None);
        assert_eq!(sanitize("带\n换行"), None);
        // 保留设备名:这种文件在 Windows 上删不掉也读不出
        assert_eq!(sanitize("NUL"), None);
        assert_eq!(sanitize("con"), None);
        assert_eq!(sanitize("COM1"), None);
        assert_eq!(sanitize("LPT9"), None);
        // 但前缀相同的正常名字不能误伤
        assert_eq!(sanitize("COM10").as_deref(), Some("COM10"));
        assert_eq!(sanitize("console").as_deref(), Some("console"));
    }

    #[test]
    fn sanitize_trims_trailing_dots_and_spaces() {
        // Windows 存盘时会吃掉结尾的点和空格,不处理的话「存的名字」和
        // 「列出来的名字」对不上,列表里会冒出两个看着一样的项
        assert_eq!(sanitize("笔记.").as_deref(), Some("笔记"));
        assert_eq!(sanitize("笔记 ").as_deref(), Some("笔记"));
        assert_eq!(sanitize("笔记... ").as_deref(), Some("笔记"));
    }

    #[test]
    fn sanitize_caps_length() {
        assert!(sanitize(&"长".repeat(MAX_NAME)).is_some());
        assert_eq!(sanitize(&"长".repeat(MAX_NAME + 1)), None);
    }

    #[test]
    fn unique_name_counts_up() {
        assert_eq!(unique_name(&[]), "新建笔记");
        assert_eq!(unique_name(&["别的".into()]), "新建笔记");
        assert_eq!(unique_name(&["新建笔记".into()]), "新建笔记 2");
        assert_eq!(
            unique_name(&["新建笔记".into(), "新建笔记 2".into()]),
            "新建笔记 3"
        );
    }

    #[test]
    fn list_only_takes_md_files() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("a.md"), "").unwrap();
        std::fs::write(dir.join("b.MD"), "").unwrap();
        std::fs::write(dir.join("c.txt"), "不是笔记").unwrap();
        std::fs::write(dir.join(".hidden.md"), "").unwrap();
        std::fs::create_dir(dir.join("子目录.md")).unwrap(); // 目录就算叫 .md 也不算

        assert_eq!(list(&dir), vec!["a".to_string(), "b".to_string()]);
        assert!(list(&dir.join("不存在")).is_empty()); // 库还没建,不是错误
    }

    #[test]
    fn create_rename_remove_round_trip() {
        let dir = temp_dir("crud");
        let first = create(&dir).unwrap();
        assert_eq!(first, "新建笔记");
        let second = create(&dir).unwrap();
        assert_eq!(second, "新建笔记 2");
        assert_eq!(list(&dir).len(), 2);

        write(&dir, &second, "# 内容").unwrap();
        assert_eq!(read(&dir, &second), "# 内容");

        let now = rename(&dir, &second, "会议记录").unwrap();
        assert_eq!(now, "会议记录");
        assert_eq!(read(&dir, "会议记录"), "# 内容");
        assert_eq!(list(&dir), vec!["会议记录".to_string(), "新建笔记".to_string()]);

        // 撞名要报错而不是覆盖
        assert!(rename(&dir, "会议记录", &first).is_err());
        // 非法名字也要报错
        assert!(rename(&dir, "会议记录", "../坏").is_err());

        remove(&dir, "会议记录").unwrap();
        assert_eq!(list(&dir), vec!["新建笔记".to_string()]);
    }

    #[test]
    fn rename_to_same_name_is_a_noop() {
        let dir = temp_dir("same");
        let name = create(&dir).unwrap();
        assert_eq!(rename(&dir, &name, &name).unwrap(), name);
        assert_eq!(list(&dir).len(), 1);
    }

    #[test]
    fn most_recent_prefers_the_last_touched() {
        let dir = temp_dir("recent");
        std::fs::write(dir.join("旧.md"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("新.md"), "").unwrap();
        assert_eq!(most_recent(&dir).as_deref(), Some("新"));
        assert_eq!(most_recent(&dir.join("空目录")), None);
    }

    #[test]
    fn path_in_stays_inside_the_library() {
        let dir = Path::new("/库");
        assert_eq!(path_in(dir, "笔记"), Some(dir.join("笔记.md")));
        assert_eq!(path_in(dir, "../todos"), None);
    }
}
