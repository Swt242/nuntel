//! 代码块的**语法高亮**。
//!
//! 用 tree-sitter:先把源码解析成语法树,再跑一份 **highlights query**(各语言语法包
//! 自带的那份 `highlights.scm`),把节点打上 `@keyword` / `@string` / `@comment`
//! 这类捕获名;我们只负责把捕获名翻成**类别**,颜色留给 Slint 那边给 —— 主题令牌在
//! Slint 里派生是这个项目的规矩(见 CLAUDE.md 的「几何与主题」)。
//!
//! ⚠️ **不往 markdown 里拼 `<font color>`**,两条理由:
//! 1. 配色会跑到 Rust 里来,三套外观 × 亮暗就得在这儿再维护一份,迟早和主题对不上;
//! 2. 代码里的 `*` `_` `` ` `` `<` 全得转义,漏一个整块样式就崩(而且 Slint 的
//!    `StyledText` 本来就认不全这些)。
//!
//! **认不出的语言原样单色返回,绝不报错**:代码块没有颜色可以接受,显示不出内容是事故。
//!
//! 解析 + 跑 query 都不便宜(毫秒级),所以调用方**必须**缓存 —— 见
//! `State::code_lines`(流式回复每个 tick 都会重切块)。
//!
//! 纯逻辑,能单测(不碰 UI)。

/// 一段文字的类别。
///
/// **故意只有这几类**:够看就行。类别一多,三套外观 × 亮暗就得各配一遍色,
/// 维护成本比多出来的那点可读性值钱。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenClass {
    Plain,
    Keyword,
    /// 字符串(含字符字面量、转义)
    Str,
    Comment,
    Number,
    Function,
    /// 类型名 / 类名 / 结构体名
    Type,
}

impl TokenClass {
    /// 给 UI 的编码。**和 ui/widgets.slint 里 `MdCodeSpan.class` 的说明对齐**,
    /// 改这里就得改那边(以及 `Theme.code-color`)。
    pub fn code(self) -> i32 {
        match self {
            TokenClass::Plain => 0,
            TokenClass::Keyword => 1,
            TokenClass::Str => 2,
            TokenClass::Comment => 3,
            TokenClass::Number => 4,
            TokenClass::Function => 5,
            TokenClass::Type => 6,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => TokenClass::Keyword,
            2 => TokenClass::Str,
            3 => TokenClass::Comment,
            4 => TokenClass::Number,
            5 => TokenClass::Function,
            6 => TokenClass::Type,
            _ => TokenClass::Plain,
        }
    }
}

/// 一行里的一段同色文字
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub class: TokenClass,
}

/// 一行(空行会给一个空格 —— 布局里那一行不然会塌成 0 高)
pub type Line = Vec<Span>;

/// 把一段代码高亮成「按行、按段」的结构。认不出的语言 = 每行一整段 Plain。
pub fn highlight(lang: &str, code: &str) -> Vec<Line> {
    let Some((language, query)) = grammar(lang) else { return plain(code) };
    let Some(classes) = byte_classes(&language, &query, code) else { return plain(code) };
    split_lines(code, &classes)
}

/// 单色版本:每行一整段(空行给一个空格,保住行高)
fn plain(code: &str) -> Vec<Line> {
    code.split('\n')
        .map(|line| {
            vec![Span {
                text: if line.is_empty() { " ".to_string() } else { line.to_string() },
                class: TokenClass::Plain,
            }]
        })
        .collect()
}

/// 把「每个字节属于哪一类」按行切成段。
///
/// 用 `char_indices` 而不是裸字节下标:类别只在**语法节点边界**上变,那一定落在字符
/// 边界上,但代码里永远别赌这个 —— 切在 UTF-8 字符中间是要 panic 的。
fn split_lines(code: &str, classes: &[u8]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut offset = 0usize; // 这一行第一个字节在整段代码里的位置
    for line in code.split('\n') {
        let mut spans: Line = Vec::new();
        let mut run: Option<(usize, TokenClass)> = None; // (这段的起点, 类别)
        for (i, _) in line.char_indices() {
            let class = TokenClass::from_code(classes.get(offset + i).copied().unwrap_or(0));
            match run {
                // 同类就继续攒
                Some((_, prev)) if prev == class => {}
                // 换类了:把上一段收掉,开新的一段
                Some((start, prev)) => {
                    spans.push(Span { text: line[start..i].to_string(), class: prev });
                    run = Some((i, class));
                }
                None => run = Some((i, class)),
            }
        }
        // 最后一段(循环结束时没有「下一个字符」来触发收尾,得在这儿补上)
        if let Some((start, class)) = run {
            spans.push(Span { text: line[start..].to_string(), class });
        }
        if spans.is_empty() {
            // 空行:给一个空格,不然 HorizontalLayout 里这一行的高度是 0
            spans.push(Span { text: " ".to_string(), class: TokenClass::Plain });
        }
        lines.push(spans);
        offset += line.len() + 1; // +1 = 那个 '\n'
    }
    lines
}

/// 每个字节属于哪个类别(0 = Plain)。
///
/// 中途任何一步失败都返回 `None`,让调用方退回单色 —— 高亮是锦上添花,
/// 不能因为它把代码块搞没了。
fn byte_classes(language: &tree_sitter::Language, query_src: &str, code: &str) -> Option<Vec<u8>> {
    // `matches()` 返回的是 StreamingIterator(不是 std 的 Iterator),得把这个 trait 带进来
    use tree_sitter::StreamingIterator;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(language).ok()?;
    let tree = parser.parse(code, None)?;
    let query = tree_sitter::Query::new(language, query_src).ok()?;
    let names = query.capture_names();

    let mut classes = vec![0u8; code.len()];
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), code.as_bytes());
    // ⚠️ query 里的 `#match?` / `#eq?` 这类断言**没有求值**(Rust 绑定把这件事留给
    // 调用方)。跳过它们的后果是偶尔多染一小段,不会错位 —— 不值得为它引一套断言求值。
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let class = class_of(names[capture.index as usize]);
            if class == TokenClass::Plain {
                continue;
            }
            let start = capture.node.start_byte().min(classes.len());
            let end = capture.node.end_byte().min(classes.len());
            // 后出现的覆盖先出现的:query 里靠后的捕获更「内层」
            // (比如字符串里的转义),正好是想要的优先级
            for byte in &mut classes[start..end] {
                *byte = class.code() as u8;
            }
        }
    }
    Some(classes)
}

/// 捕获名 → 类别。
///
/// 捕获名是**带点的层级名**(`keyword.control`、`string.escape`),所以按第一段判。
fn class_of(capture: &str) -> TokenClass {
    match capture.split('.').next().unwrap_or("") {
        "keyword" | "conditional" | "repeat" | "exception" | "include" | "storage" => {
            TokenClass::Keyword
        }
        "string" | "char" | "escape" => TokenClass::Str,
        "comment" => TokenClass::Comment,
        "number" | "float" | "integer" | "boolean" => TokenClass::Number,
        // `true` / `false` / 常量:借数字那一档颜色(One Dark 里它俩本来就同色)
        "constant" => TokenClass::Number,
        "function" | "method" | "constructor" => TokenClass::Function,
        "type" | "class" | "interface" | "enum" | "struct" | "union" | "namespace" | "module" => {
            TokenClass::Type
        }
        // `variable` / `property` / `operator` / `punctuation` …:代码里绝大多数 token
        // 都是这些,**保持原色才不至于花**。
        _ => TokenClass::Plain,
    }
}

/// 语言名 → (语法, highlights query)。
///
/// ⚠️ **各个语法包导出的常量名不统一**(实测):
/// - javascript 和 bash 是 `HIGHLIGHT_QUERY`(没有 S),别的多半是 `HIGHLIGHTS_QUERY`;
/// - typescript 一个包两个语言:`LANGUAGE_TYPESCRIPT` / `LANGUAGE_TSX`。
///
/// 加语言时照抄一行、**别猜**,猜错了编译期就报错(这是好事)。
fn grammar(lang: &str) -> Option<(tree_sitter::Language, String)> {
    let lang = lang.trim().to_ascii_lowercase();
    let js = tree_sitter_javascript::HIGHLIGHT_QUERY;
    Some(match lang.as_str() {
        "rust" | "rs" => {
            (tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY.to_string())
        }
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => {
            (tree_sitter_javascript::LANGUAGE.into(), js.to_string())
        }
        // ⚠️ **TypeScript 的 query 只列了 TS 特有的关键字**(`abstract` / `interface` /
        // `keyof` …),JS 那一批(`const` / `let` / `function` …)得靠**拼上 JS 的 query**。
        // 上游那份 .scm 开头本来写着 `; inherits: javascript`,但 0.23.2 的
        // `queries/highlights.scm` 里**没有这一行**(实测),而 tree-sitter 的 Rust 绑定
        // 也不会自己去解析继承关系 —— 不拼的结果就是 TS 代码里 `const` 不上色。
        "typescript" | "ts" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            format!("{js}\n{}", tree_sitter_typescript::HIGHLIGHTS_QUERY),
        ),
        "tsx" => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            format!("{js}\n{}", tree_sitter_typescript::HIGHLIGHTS_QUERY),
        ),
        "python" | "py" => {
            (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY.to_string())
        }
        "json" => {
            (tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY.to_string())
        }
        "bash" | "sh" | "shell" | "zsh" => {
            (tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY.to_string())
        }
        "go" | "golang" => {
            (tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY.to_string())
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一行拼回去 —— 高亮**不许丢字符、不许改字符**,这是最基本的约定
    fn line_text(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    fn classes_of(lines: &[Line], needle: &str) -> Vec<TokenClass> {
        lines
            .iter()
            .flat_map(|l| l.iter())
            .filter(|s| s.text == needle)
            .map(|s| s.class)
            .collect()
    }

    #[test]
    fn rust_keywords_and_strings_are_classified() {
        let lines = highlight("rust", "fn main() {\n    let s = \"hi\";\n}");
        assert_eq!(lines.len(), 3);
        // 拼回去必须和原文一字不差
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(joined.join("\n"), "fn main() {\n    let s = \"hi\";\n}");
        assert_eq!(classes_of(&lines, "fn"), vec![TokenClass::Keyword]);
        assert_eq!(classes_of(&lines, "let"), vec![TokenClass::Keyword]);
        assert_eq!(classes_of(&lines, "\"hi\""), vec![TokenClass::Str]);
    }

    #[test]
    fn comments_are_classified() {
        let lines = highlight("python", "# 注释\nx = 1");
        assert_eq!(classes_of(&lines, "# 注释"), vec![TokenClass::Comment]);
        assert_eq!(classes_of(&lines, "1"), vec![TokenClass::Number]);
    }

    #[test]
    fn javascript_and_typescript_use_their_own_queries() {
        // 这两个包的常量名和别的不一样(HIGHLIGHT_QUERY),这条测试盯着别写错
        let js = highlight("js", "const a = 1;");
        assert_eq!(classes_of(&js, "const"), vec![TokenClass::Keyword]);
        let ts = highlight("ts", "const b: number = 2;");
        assert_eq!(classes_of(&ts, "const"), vec![TokenClass::Keyword]);
        let tsx = highlight("tsx", "const c = <div />;");
        assert_eq!(classes_of(&tsx, "const"), vec![TokenClass::Keyword]);
    }


    #[test]
    fn unknown_language_falls_back_to_plain() {
        let lines = highlight("brainfuck", "+++[->++<]");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].class, TokenClass::Plain);
        assert_eq!(line_text(&lines[0]), "+++[->++<]");
    }

    #[test]
    fn empty_lines_keep_their_height() {
        // 空行不能是「一段空文字」,不然布局里那一行高度是 0,代码会挤在一起
        let lines = highlight("rust", "a\n\nb");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].iter().all(|s| !s.text.is_empty()));
    }

    #[test]
    fn weird_input_does_not_panic() {
        // 不完整的代码(流式回复里常见):不能 panic,也不能丢字符
        for (lang, code) in [
            ("rust", "fn main() {"),
            ("python", "def f(:"),
            ("json", "{\"a\": "),
            ("bash", "if [ -f"),
            ("go", "func f() {"),
            ("typescript", "const x: = "),
            ("", "没有语言名"),
        ] {
            let lines = highlight(lang, code);
            assert_eq!(lines.iter().map(line_text).collect::<Vec<_>>().join("\n"), code);
        }
        // 空代码段:给一行、一个空格(保住行高),不该 panic
        let lines = highlight("rust", "");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), " ");
    }
}
