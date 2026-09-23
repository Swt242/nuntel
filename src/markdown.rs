//! Markdown 的**块级**切分。
//!
//! 为什么只切块:Slint 自带的 `StyledText` 元素已经能按 CommonMark 渲染**行内**样式
//! (加粗/斜体/删除线/行内代码/链接/有序无序列表,外加 `<u>` `<font color>` 两个 HTML 标签),
//! 而且 `slint::StyledText::from_markdown()` 能在 Rust 侧把字符串解析成它要的类型。
//! 但它**不支持标题、代码块、引用、表格、分隔线**(见 Slint 文档的 StyledText 页)。
//!
//! 所以这里的分工是:
//! - **这里**:把源码切成「段落 / 标题 / 代码块 / 引用 / 分隔线 / 表格 / 图片 / 任务列表」
//!   这些块;
//! - **StyledText**:负责块**内部**的行内样式。
//!
//! 两个**它渲染不了、只能在这里降级**的东西:
//! - **行内图片**(夹在句子中间的 `![alt](路径)`):Slint 没法把图塞进文字流,降级成 alt 文字;
//! - 单独成行的图片才出一个 `Image` 块,交给 UI 画。
//!
//! 任务列表要**带源码行号**,因为预览里点勾选框是**回去改源码**(不是只改显示)。
//!
//! 这样既不用引 pulldown-cmark(块级那点规则自己切足够),也不用自己写行内解析器。
//!
//! 纯函数,没有任何依赖,能直接单测 —— 和 `model.rs` / `reminder.rs` / `pet.rs` 一个路子。

/// 一个渲染块。UI 按 `kind` 挑对应的控件画。
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// 普通段落 —— 也可能是一段列表(`- ` / `1. ` 开头的连续行),
    /// 列表交给 StyledText 自己认,这里不拆
    Rich(String),
    /// 标题。`level` 1..6,UI 按级别给字号
    Heading { level: u8, text: String },
    /// 围栏代码块。`lang` 是 ``` 后面跟的语言名(没有就是空串)
    Code { lang: String, code: String },
    /// 引用(连续 `> ` 行合成一段)
    Quote(String),
    /// 分隔线 `---`
    Rule,
    /// 表格。**每个元素是一行,行里是各单元格** —— 在 Rust 侧就切好,
    /// UI 才能用 GridLayout 画出真正对齐的表格(StyledText 不支持表格)。
    /// `aligns` 是每列的对齐,来自分隔行里的冒号(`:---` / `:---:` / `---:`),
    /// 长度一定等于列数(列数对不上就当不成表格,见 `table_sep_aligns`)。
    Table { rows: Vec<Vec<String>>, aligns: Vec<Align> },
    /// 单独成行的图片:`![alt](路径)`(可选的 `"标题"` 认了但不用)。
    ///
    /// ⚠️ **只有「整行就是一张图」才出这个块**。夹在句子中间的行内图片**渲染不了**
    /// (Slint 没法把图塞进文字流里),那边降级成 alt 文字,见 `strip_inline_images`。
    Image { alt: String, url: String },
    /// 任务列表:`- [ ]` / `- [x]`,连续成组的行合成一块。
    ///
    /// 每项带着**源码行号** —— 预览里点勾选框要照着它回去改源码(不是只改显示)。
    Tasks(Vec<TaskItem>),
}

/// 表格某一列的对齐方式(分隔行里冒号的位置决定)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// 任务列表里的一项
#[derive(Debug, Clone, PartialEq)]
pub struct TaskItem {
    pub done: bool,
    pub text: String,
    /// 这一项在**源码里的行号**(0 起)。回写「勾上/取消」要用它定位。
    pub line: usize,
}

/// 把 Markdown 源码切成块。
///
/// 对**不完整**的输入要宽容(流式回复时经常只有半截):围栏没闭合就当成代码块到结尾,
/// 标题没有内容就当空标题 —— 宁可画得朴素点,也不要吞掉半段内容或 panic。
pub fn parse(src: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = src.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;

    // 段落缓冲:连续的普通行攒成一块,遇到空行或另一种块就收
    let mut para: Vec<String> = Vec::new();
    macro_rules! flush_para {
        () => {
            if !para.is_empty() {
                blocks.push(Block::Rich(strip_inline_images(&para.join("\n"))));
                para.clear();
            }
        };
    }

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // 空行:段落断在这里
        if trimmed.is_empty() {
            flush_para!();
            i += 1;
            continue;
        }

        // 围栏代码块 ``` 或 ~~~
        if let Some(fence) = fence_marker(trimmed) {
            flush_para!();
            let lang = trimmed.trim_start_matches(fence).trim().to_string();
            let mut code: Vec<&str> = Vec::new();
            i += 1;
            let mut closed = false;
            while i < lines.len() {
                let l = lines[i].trim();
                if l.starts_with(fence) && l.trim_start_matches(fence).trim().is_empty() {
                    closed = true;
                    i += 1;
                    break;
                }
                code.push(lines[i]);
                i += 1;
            }
            // 没闭合也照样出块(流式时就是半截),别把内容丢了
            let _ = closed;
            // 去掉尾部空行:``` 前面那行空白不该算进代码里
            while code.last().is_some_and(|l| l.trim().is_empty()) {
                code.pop();
            }
            blocks.push(Block::Code {
                lang,
                code: code.join("\n"),
            });
            continue;
        }

        // 分隔线:--- / *** / ___(三个以上,允许中间有空格)
        if is_rule(trimmed) {
            flush_para!();
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }

        // 标题 # .. ######
        if let Some((level, text)) = heading(trimmed) {
            flush_para!();
            blocks.push(Block::Heading { level, text: strip_inline_images(&text) });
            i += 1;
            continue;
        }

        // 引用:连续的 > 行合成一段
        if trimmed.starts_with('>') {
            flush_para!();
            let mut quoted: Vec<&str> = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim();
                // 引用里允许空行(用 `>` 单独一行表示),但完全空行就结束
                if let Some(rest) = l.strip_prefix('>') {
                    quoted.push(rest.strip_prefix(' ').unwrap_or(rest));
                    i += 1;
                } else {
                    break;
                }
            }
            let text = strip_inline_images(quoted.join("\n").trim());
            blocks.push(Block::Quote(text));
            continue;
        }

        // 单独成行的图片(排在表格前面:`![a](b)` 里不会有竖线,顺序其实无所谓,
        // 但任务列表那一支必须排在表格前面 —— `- [x] 甲 | 乙` 得算任务不算表格)
        if let Some((alt, url)) = image_line(trimmed) {
            flush_para!();
            blocks.push(Block::Image { alt, url });
            i += 1;
            continue;
        }

        // 任务列表:连续成组的 `- [ ]` / `- [x]` 合成一块(中间夹了别的行就断开)
        if task_line(trimmed).is_some() {
            flush_para!();
            let mut items = Vec::new();
            while i < lines.len() {
                let Some((done, text)) = task_line(lines[i].trim()) else { break };
                items.push(TaskItem { done, text: strip_inline_images(&text), line: i });
                i += 1;
            }
            blocks.push(Block::Tasks(items));
            continue;
        }

        // 表格:当前行带竖线、下一行是分隔行(|---|:--:|),**且两者列数一致**才认定。
        //
        // 几个细节都是从真实输入里抠出来的:
        // - 首尾竖线可以省略(Markdown 允许 `A | B` 这种写法),所以不能要求 starts_with('|');
        // - 要求列数一致,是为了别把「一行带竖线的普通文字 + 下一行是 ---」误判成表格
        //   (分隔行的 `---` 本身也是「一格全横线」,不比对列数就会误判);
        // - 后面的数据行同理,必须带竖线才收。
        let table_aligns = if trimmed.contains('|') && i + 1 < lines.len() {
            table_sep_aligns(lines[i + 1].trim(), split_row(trimmed).len())
        } else {
            None
        };
        if let Some(mut aligns) = table_aligns {
            flush_para!();
            let mut rows: Vec<Vec<String>> = vec![split_row(lines[i])];
            i += 2; // 跳过表头行和分隔行
            while i < lines.len() && lines[i].trim().contains('|') {
                rows.push(split_row(lines[i]));
                i += 1;
            }
            // 各行列数补齐:Markdown 允许少写几格,补空串,免得 UI 里网格缺角
            let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            for row in rows.iter_mut() {
                row.resize(cols, String::new());
                // 单元格是纯文本(不走 StyledText),行内图片在这里降级成 alt
                for cell in row.iter_mut() {
                    *cell = strip_inline_images(cell);
                }
            }
            // 分隔行少写几格时(行本身合法但列数不同)会被 `table_sep_aligns` 挡掉,
            // 走到这儿 aligns 一定够长;补齐只是防手滑
            aligns.resize(cols, Align::Left);
            blocks.push(Block::Table { rows, aligns });
            continue;
        }

        // 其余:并进当前段落
        para.push(line.to_string());
        i += 1;
    }
    flush_para!();
    blocks
}

/// 这一行是不是围栏标记(``` 或 ~~~,三个起步),返回用哪个字符当围栏
fn fence_marker(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `# ` 到 `###### ` → (级别, 文本)。要求 # 后面跟空格或行尾,
/// 否则 `#tag` 这种会被误当成标题
fn heading(trimmed: &str) -> Option<(u8, String)> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((hashes as u8, rest.trim().to_string()))
}

/// 分隔线:三个以上 `-` / `*` / `_`,中间可以有空格
fn is_rule(trimmed: &str) -> bool {
    let mut marker = None;
    let mut count = 0;
    for c in trimmed.chars() {
        match c {
            '-' | '*' | '_' => {
                if *marker.get_or_insert(c) != c {
                    return false; // 混用了不同符号
                }
                count += 1;
            }
            ' ' | '\t' => {}
            _ => return false, // 有别的字符就不是分隔线(比如 `- 列表项`)
        }
    }
    count >= 3
}

/// 把表格的一行切成单元格。
///
/// 去掉首尾那两根竖线,再按 `|` 切、逐个 trim。转义的 `\|` 不处理 ——
/// AI 回复里基本不会出现,真出现了也就是多一列,不影响读。
fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// 表格的分隔行:`|---|---|` 或 `|:--:|` 这种(首尾竖线可有可无),
/// 顺带把**每列的对齐**解析出来。返回 `None` = 不是合法分隔行,或者列数跟表头对不上。
///
/// `expect_cols` 是表头那行的列数:必须**对得上**才算表格 ——
/// 否则「一段带竖线的普通文字」后面跟一行 `---`(分隔线)也会被认成表格。
fn table_sep_aligns(trimmed: &str, expect_cols: usize) -> Option<Vec<Align>> {
    let cells = split_row(trimmed);
    if cells.is_empty() || cells.len() != expect_cols {
        return None;
    }
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in &cells {
        let cell = cell.trim();
        let dashes = cell.trim_matches(':');
        if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
            return None;
        }
        // 两头的冒号决定对齐:`:---:` 居中、`---:` 靠右、其余(含 `:---`)靠左
        aligns.push(match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(aligns)
}

/// 整行就是一张图片:`![alt](路径)`,后面可以再跟一个 `"标题"`(认了,但不用)。
///
/// **只有整行才算** —— 前后还有别的字就是行内图片,那条路走 `strip_inline_images`。
fn image_line(trimmed: &str) -> Option<(String, String)> {
    let rest = trimmed.strip_prefix("![")?;
    let (alt, rest) = rest.split_once("](")?;
    let rest = rest.strip_suffix(')')?;
    // 路径取第一个空白之前那一段,空白后面是可选标题
    let url = rest.split_whitespace().next().unwrap_or("");
    if url.is_empty() {
        return None;
    }
    Some((alt.to_string(), url.to_string()))
}

/// 任务列表的一行:`- [ ] 文字` / `- [x] 文字`(也认 `*` `+` 和大写 `X`)。
/// 返回 `(勾没勾, 后面的文字)`。
fn task_line(trimmed: &str) -> Option<(bool, String)> {
    let rest = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.chars();
    let done = match chars.next()? {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let rest = chars.as_str().strip_prefix(']')?;
    // `]` 后面必须是空格或者行尾,免得把 `[x]y` 这种也算成任务
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((done, rest.trim_start().to_string()))
}

/// 任务行里那个**标记字符**(`[ ]` 的空格、或 `[x]` 的 x)在整行里的字节位置。
///
/// 认缩进、认 `-`/`*`/`+`;别的一律 `None`。
fn task_mark_offset(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    // 先跳过行首空白(缩进的子任务也要能点)
    let mut i = line.len() - line.trim_start().len();
    match *bytes.get(i)? {
        b'-' | b'*' | b'+' => i += 1,
        _ => return None,
    }
    while bytes.get(i) == Some(&b' ') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    match *bytes.get(i)? {
        b' ' | b'x' | b'X' => Some(i),
        _ => None,
    }
}

/// 把**某一行**任务勾选框翻过来(`[ ]` ↔ `[x]`),返回改过的新源码。
///
/// 行号越界、或者那一行已经不是任务了,返回 `None` —— 预览里点一下的时候,
/// 用户可能正在编辑器里改这一行,这种「对不上」的情况什么都不做最安全。
///
/// 只动方括号里那**一个字符**:整行的缩进、空格、后面的正文一个字节都不碰
/// (源码是用户的,别顺手规范化)。
pub fn toggle_task_line(src: &str, line: usize) -> Option<String> {
    let mut out = String::with_capacity(src.len() + 1);
    let mut hit = false;
    for (i, text) in src.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if i != line {
            out.push_str(text);
            continue;
        }
        let at = task_mark_offset(text)?;
        let mark = if text.as_bytes()[at] == b' ' { 'x' } else { ' ' };
        out.push_str(&text[..at]);
        out.push(mark);
        out.push_str(&text[at + 1..]);
        hit = true;
    }
    hit.then_some(out)
}

/// 把**行内**图片 `![alt](路径)` 降级成 `alt`。
///
/// 为什么非降级不可:行内样式是交给 Slint 的 `StyledText` 的,而它**不认图片**,
/// 不处理的话页面上会直接露出一串 `![说明](路径)` 原文 —— 比没有图更难看。
/// 渲染不了图,至少把说明文字留给人看(和 GitHub 图片挂了显示 alt 是一个意思)。
///
/// 只认最简单的一层,不做嵌套括号。**输入不完整时原样保留**(AI 流式回复经常只有
/// 半截 `![`),等下一轮解析出完整的再降级。
fn strip_inline_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("![") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some((alt, tail)) = after.split_once("](") {
            if let Some(end) = tail.find(')') {
                out.push_str(alt);
                rest = &tail[end + 1..];
                continue;
            }
        }
        out.push_str("![");
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_headings_by_level() {
        let blocks = parse("# 一级\n## 二级\n###### 六级");
        assert_eq!(
            blocks,
            vec![
                Block::Heading { level: 1, text: "一级".into() },
                Block::Heading { level: 2, text: "二级".into() },
                Block::Heading { level: 6, text: "六级".into() },
            ]
        );
    }

    #[test]
    fn hash_without_space_is_not_a_heading() {
        // `#tag` / `#标题`(没有空格)不该被当成标题 —— AI 写话题标签很常见
        let blocks = parse("#tag 和 #另一件事");
        assert_eq!(blocks, vec![Block::Rich("#tag 和 #另一件事".into())]);
        // 但 `# ` 后面直接换行(空标题)要认
        assert_eq!(parse("#").len(), 1);
        assert!(matches!(parse("#")[0], Block::Heading { level: 1, .. }));
    }

    #[test]
    fn collects_fenced_code_and_keeps_language() {
        let blocks = parse("前\n```rust\nlet x = 1;\n*不是斜体*\n```\n后");
        assert_eq!(
            blocks,
            vec![
                Block::Rich("前".into()),
                Block::Code { lang: "rust".into(), code: "let x = 1;\n*不是斜体*".into() },
                Block::Rich("后".into()),
            ]
        );
    }

    #[test]
    fn unclosed_fence_still_becomes_a_block() {
        // 流式回复里很常见:``` 刚打出来,内容还没到
        let blocks = parse("说明\n```python\nprint(1)");
        assert_eq!(
            blocks,
            vec![
                Block::Rich("说明".into()),
                Block::Code { lang: "python".into(), code: "print(1)".into() },
            ]
        );
    }

    #[test]
    fn merges_consecutive_quote_lines() {
        let blocks = parse("> 第一行\n> 第二行\n\n正文");
        assert_eq!(
            blocks,
            vec![Block::Quote("第一行\n第二行".into()), Block::Rich("正文".into())]
        );
    }

    #[test]
    fn recognises_rules_but_not_list_items() {
        assert_eq!(parse("---"), vec![Block::Rule]);
        assert_eq!(parse("***"), vec![Block::Rule]);
        assert_eq!(parse("- - -"), vec![Block::Rule]);
        // `- 列表项` 不是分隔线
        assert_eq!(parse("- 列表项"), vec![Block::Rich("- 列表项".into())]);
        // 混用符号不算
        assert_eq!(parse("-*-"), vec![Block::Rich("-*-".into())]);
    }

    #[test]
    fn keeps_lists_as_one_rich_block() {
        // 列表交给 StyledText 自己认,这里整段一起给
        let blocks = parse("- 甲\n- 乙\n1. 丙");
        assert_eq!(blocks, vec![Block::Rich("- 甲\n- 乙\n1. 丙".into())]);
    }

    #[test]
    fn splits_paragraphs_on_blank_lines() {
        let blocks = parse("第一段\n还是第一段\n\n第二段");
        assert_eq!(
            blocks,
            vec![Block::Rich("第一段\n还是第一段".into()), Block::Rich("第二段".into())]
        );
    }

    #[test]
    fn recognises_tables() {
        let src = "| 列 A | 列 B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let blocks = parse(src);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows.len(), 3, "表头 + 两行数据:{rows:?}");
                assert_eq!(rows[0], vec!["列 A", "列 B"]);
                assert_eq!(rows[2], vec!["3", "4"]);
                // 分隔行不该混进来
                assert!(!rows.iter().flatten().any(|c| c.contains("---")));
            }
            other => panic!("不是表格: {other:?}"),
        }
    }

    /// 少写几格的表格(Markdown 允许)要补齐,不然 UI 里网格会缺角
    #[test]
    fn pads_short_table_rows() {
        let src = "| A | B | C |\n|---|---|---|\n| 1 |\n| 1 | 2 | 3 |";
        match &parse(src)[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows[1], vec!["1", "", ""]);
                assert_eq!(rows[2], vec!["1", "2", "3"]);
            }
            other => panic!("不是表格: {other:?}"),
        }
    }

    /// 首尾竖线可以不写
    #[test]
    fn table_without_outer_pipes() {
        let src = "A | B\n---|---\n1 | 2";
        match &parse(src)[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows[0], vec!["A", "B"]);
                assert_eq!(rows[1], vec!["1", "2"]);
            }
            other => panic!("不是表格: {other:?}"),
        }
    }

    /// 「一行带竖线的普通文字」后面跟一行 `---`(分隔线)—— 不能误判成表格
    #[test]
    fn pipe_text_followed_by_rule_is_not_a_table() {
        let blocks = parse("说明 | 备注
---");
        assert_eq!(blocks.len(), 2, "{blocks:#?}");
        assert!(matches!(blocks[0], Block::Rich(_)));
        assert_eq!(blocks[1], Block::Rule);
    }

    #[test]
    fn pipe_line_without_separator_is_not_a_table() {
        // 只是一行带竖线的文字,不该当表格
        assert_eq!(parse("| 就是一句话 |"), vec![Block::Rich("| 就是一句话 |".into())]);
    }

    #[test]
    fn handles_crlf_and_empty_input() {
        assert_eq!(parse(""), Vec::<Block>::new());
        assert_eq!(parse("\n\n\n"), Vec::<Block>::new());
        assert_eq!(parse("# 标题\r\n正文\r\n"), vec![
            Block::Heading { level: 1, text: "标题".into() },
            Block::Rich("正文".into()),
        ]);
    }

    #[test]
    fn mixed_document_keeps_order() {
        let src = "# 标题\n\n段落一\n\n> 引用\n\n```\ncode\n```\n\n---\n\n- 列表";
        let blocks = parse(src);
        assert_eq!(blocks.len(), 6, "{blocks:#?}");
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Rich(_)));
        assert!(matches!(blocks[2], Block::Quote(_)));
        assert!(matches!(blocks[3], Block::Code { .. }));
        assert!(matches!(blocks[4], Block::Rule));
        assert!(matches!(blocks[5], Block::Rich(_)));
    }

    /// 整行一张图 = 一个图片块(可选的 "标题" 认了但不影响 url)
    #[test]
    fn image_alone_on_a_line_becomes_a_block() {
        assert_eq!(
            parse("![示意图](pics/a.png)"),
            vec![Block::Image { alt: "示意图".into(), url: "pics/a.png".into() }]
        );
        // 前后有空白没事
        assert_eq!(
            parse("   ![图](b.jpg)  "),
            vec![Block::Image { alt: "图".into(), url: "b.jpg".into() }]
        );
        // 带标题的写法
        assert_eq!(
            parse("![图](c.png \"标题\")"),
            vec![Block::Image { alt: "图".into(), url: "c.png".into() }]
        );
        // 路径是空的:不当图片,退回普通段落(宁可显示原文也别画个空框)
        assert!(matches!(parse("![]()")[0], Block::Rich(_)));
    }

    /// 夹在句子里的行内图片渲染不了,降级成 alt 文字 ——
    /// 关键是**别把 `![说明](路径)` 原文露给用户**
    #[test]
    fn inline_image_falls_back_to_alt_text() {
        assert_eq!(
            parse("看这张 ![示意图](a.png) 就懂了"),
            vec![Block::Rich("看这张 示意图 就懂了".into())]
        );
        // 引用和标题里也一样
        assert_eq!(parse("> 见图 ![图](b.png)"), vec![Block::Quote("见图 图".into())]);
        assert_eq!(
            parse("# 标题 ![图](c.png)"),
            vec![Block::Heading { level: 1, text: "标题 图".into() }]
        );
    }

    /// 流式回复里经常只有半截 `![`,这时候原样留着,等下一轮再降级
    #[test]
    fn unfinished_inline_image_is_left_alone() {
        assert_eq!(parse("见图 ![图](a.p"), vec![Block::Rich("见图 ![图](a.p".into())]);
        assert_eq!(parse("见图 !["), vec![Block::Rich("见图 ![".into())]);
    }

    /// 任务列表合成一块,并且**带源码行号**(预览里点勾选框靠它回写源码)
    #[test]
    fn task_list_carries_source_line_numbers() {
        // 行号:0 开头 / 1 空行 / 2..4 三行任务 / 5 空行 / 6 结尾
        let blocks = parse("开头\n\n- [ ] 甲\n- [x] 乙\n* [X] 丙\n\n结尾");
        assert_eq!(blocks.len(), 3, "{blocks:#?}");
        match &blocks[1] {
            Block::Tasks(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!((items[0].done, items[0].text.as_str(), items[0].line), (false, "甲", 2));
                assert_eq!((items[1].done, items[1].text.as_str(), items[1].line), (true, "乙", 3));
                assert_eq!((items[2].done, items[2].text.as_str(), items[2].line), (true, "丙", 4));
            }
            other => panic!("不是任务列表: {other:?}"),
        }
    }

    /// `[x]` 后面没空格、或者方括号里是别的东西 —— 都不算任务
    #[test]
    fn task_list_needs_the_exact_shape() {
        assert_eq!(parse("- [x]紧挨着"), vec![Block::Rich("- [x]紧挨着".into())]);
        assert_eq!(parse("- [-] 没有这种"), vec![Block::Rich("- [-] 没有这种".into())]);
        assert_eq!(parse("- [ ]"), vec![Block::Tasks(vec![TaskItem {
            done: false,
            text: String::new(),
            line: 0,
        }])]);
    }

    /// 真实笔记里那种表格(前面有小标题、后面跟空行、带对齐冒号、有长单元格)
    /// —— 从验收笔记里原样抄下来的回归用例
    #[test]
    fn recognises_table_from_a_real_note() {
        let src = "## 表格\n\n| 左对齐 | 居中 | 右对齐 |\n|:---|:---:|---:|\n| 短 | 中间 | 1 |\n| 长文本长文本 | 中 | 2 |\n\n## 任务列表\n";
        let blocks = parse(src);
        // 标题 + 表格 + 标题 = 3 块(别把「## 任务列表」那次数漏)
        assert_eq!(blocks.len(), 3, "{blocks:#?}");
        assert!(matches!(blocks[1], Block::Table { .. }), "{blocks:#?}");
    }

    /// 翻勾选框:只动方括号里那一个字符,缩进和正文一个字节都不碰
    #[test]
    fn toggle_task_line_flips_only_the_mark() {
        let src = "标题\n\n- [ ] 甲\n  - [x] 乙\n* [X] 丙\n";
        assert_eq!(
            toggle_task_line(src, 2).unwrap(),
            "标题\n\n- [x] 甲\n  - [x] 乙\n* [X] 丙\n"
        );
        assert_eq!(
            toggle_task_line(src, 3).unwrap(),
            "标题\n\n- [ ] 甲\n  - [ ] 乙\n* [X] 丙\n"
        );
        assert_eq!(
            toggle_task_line(src, 4).unwrap(),
            "标题\n\n- [ ] 甲\n  - [x] 乙\n* [ ] 丙\n"
        );
        // 行号越界、不是任务行、不是列表行 —— 都原样不动(返回 None 让调用方别理它)
        assert_eq!(toggle_task_line(src, 99), None);
        assert_eq!(toggle_task_line(src, 0), None);
        assert_eq!(toggle_task_line("普通段落", 0), None);
        assert_eq!(toggle_task_line("- [-] 没有这种", 0), None);
    }

    /// 分隔行两头的冒号决定对齐:`:---` 左、`:---:` 中、`---:` 右
    #[test]
    fn table_alignment_comes_from_the_separator() {
        let src = "| 左 | 中 | 右 |\n|:---|:---:|---:|\n| 1 | 2 | 3 |";
        match &parse(src)[0] {
            Block::Table { aligns, rows } => {
                assert_eq!(aligns, &vec![Align::Left, Align::Center, Align::Right]);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("不是表格: {other:?}"),
        }
        // 没写冒号就是左对齐(和 GitHub 一致)
        match &parse("A|B\n---|---\n1|2")[0] {
            Block::Table { aligns, .. } => assert_eq!(aligns, &vec![Align::Left, Align::Left]),
            other => panic!("不是表格: {other:?}"),
        }
    }

    #[test]
    fn inline_markup_is_left_alone() {
        // 行内样式不由这里处理 —— 原样留着,交给 StyledText
        let blocks = parse("**粗** 和 `代码` 和 [链接](https://x)");
        assert_eq!(
            blocks,
            vec![Block::Rich("**粗** 和 `代码` 和 [链接](https://x)".into())]
        );
    }
}
