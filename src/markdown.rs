//! Markdown 的**块级**切分。
//!
//! 为什么只切块:Slint 自带的 `StyledText` 元素已经能按 CommonMark 渲染**行内**样式
//! (加粗/斜体/删除线/行内代码/链接/有序无序列表,外加 `<u>` `<font color>` 两个 HTML 标签),
//! 而且 `slint::StyledText::from_markdown()` 能在 Rust 侧把字符串解析成它要的类型。
//! 但它**不支持标题、代码块、引用、表格、分隔线**(见 Slint 文档的 StyledText 页)。
//!
//! 所以这里的分工是:
//! - **这里**:把源码切成「段落 / 标题 / 代码块 / 引用 / 分隔线 / 表格」这些块;
//! - **StyledText**:负责块**内部**的行内样式。
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
    Table(Vec<Vec<String>>),
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
                blocks.push(Block::Rich(para.join("\n")));
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
            blocks.push(Block::Heading { level, text });
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
            blocks.push(Block::Quote(quoted.join("\n").trim().to_string()));
            continue;
        }

        // 表格:当前行带竖线、下一行是分隔行(|---|:--:|),**且两者列数一致**才认定。
        //
        // 几个细节都是从真实输入里抠出来的:
        // - 首尾竖线可以省略(Markdown 允许 `A | B` 这种写法),所以不能要求 starts_with('|');
        // - 要求列数一致,是为了别把「一行带竖线的普通文字 + 下一行是 ---」误判成表格
        //   (分隔行的 `---` 本身也是「一格全横线」,不比对列数就会误判);
        // - 后面的数据行同理,必须带竖线才收。
        if trimmed.contains('|')
            && i + 1 < lines.len()
            && is_table_sep(lines[i + 1].trim(), split_row(trimmed).len())
        {
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
            }
            blocks.push(Block::Table(rows));
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

/// 表格的分隔行:`|---|---|` 或 `|:--:|` 这种(首尾竖线可有可无)。
///
/// `expect_cols` 是表头那行的列数:必须**对得上**才算表格 ——
/// 否则「一段带竖线的普通文字」后面跟一行 `---`(分隔线)也会被认成表格。
fn is_table_sep(trimmed: &str, expect_cols: usize) -> bool {
    let cells = split_row(trimmed);
    if cells.is_empty() || cells.len() != expect_cols {
        return false;
    }
    cells.iter().all(|c| {
        let c = c.trim().trim_matches(':');
        !c.is_empty() && c.chars().all(|ch| ch == '-')
    })
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
            Block::Table(rows) => {
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
            Block::Table(rows) => {
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
            Block::Table(rows) => {
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
