//! OpenAI 兼容的对话接口:构造请求、解析响应。
//!
//! 这里**只管协议** —— 不碰网络、不碰 UI。请求体和响应解析都是纯函数,能直接单测;
//! 真正的网络请求在 `main.rs` 的后台线程里跑(见 `State::start_chat`)。
//!
//! 「OpenAI 兼容」是指一大票服务(OpenAI、DeepSeek、Moonshot、通义、vLLM、Ollama、
//! 各种中转站…)都实现了同一套 `/chat/completions` 协议,只要 `base_url` + `api_key`
//! + `model` 三个参数就能走通。所以这里**只按这套协议写**,不针对任何一家做特判。

use serde_json::json;

/// 对话消息的角色
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// 把用户填的地址补成完整的 `chat/completions` 地址。
///
/// 用户可能填这几种,都得能跑:
/// - `https://api.openai.com/v1`            → 补 `/chat/completions`
/// - `https://api.openai.com/v1/`           → 去掉尾部斜杠再补
/// - `http://localhost:11434/v1`            → 同上(Ollama / vLLM 本地服务)
/// - `https://xx/v1/chat/completions`       → 已经写全了,原样用
/// - `https://xx/chat/completions`          → 原样用(有的中转站不带 /v1)
///
/// **不擅自补 `/v1`**:有的服务就是挂在根路径上,替他猜反而错。
pub fn endpoint(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// 请求体。写成一遍 `json!` 而不是 serde 结构体:字段少,而且各家实现会忽略不认识的字段,
/// 这样加参数不用同步改结构体。
pub fn request_body(model: &str, messages: &[Message], stream: bool) -> serde_json::Value {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| json!({ "role": m.role.as_str(), "content": m.content }))
        .collect();
    json!({
        "model": model,
        "messages": msgs,
        "stream": stream,
    })
}

/// 一行 SSE 解析出来的东西
#[derive(Debug, PartialEq)]
pub enum Chunk {
    /// 一小段正文增量
    Text(String),
    /// `data: [DONE]`,流结束
    Done,
    /// 这行没内容(空行、注释、只有 role 的首包、只有 reasoning 的包…)
    Ignore,
    /// 服务端报错(有些实现会在流中间塞一个 error 包)
    Error(String),
}

/// 解析一行 SSE。
///
/// 只处理 OpenAI 这套约定里会出现的东西:
/// - `data: {...}` / `data:{...}`(冒号后有没有空格都行)
/// - `data: [DONE]`
/// - 以 `:` 开头的注释行(有些服务拿它当心跳)和空行 → 忽略
/// - `event:` / `id:` 等字段 → 忽略
///
/// **不做跨行的 JSON 拼接**:OpenAI 兼容实现都是「一个事件一行 JSON」,
/// 真遇到把 JSON 拆成多个 `data:` 行的服务,这里会把它当解析失败跳过(见下面的 Err 处理)。
pub fn parse_sse_line(line: &str) -> Chunk {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some(rest) = line.strip_prefix("data:") else {
        return Chunk::Ignore; // 空行、注释、event:/id: 之类
    };
    let payload = rest.trim();
    if payload.is_empty() {
        return Chunk::Ignore;
    }
    if payload == "[DONE]" {
        return Chunk::Done;
    }

    // 解析失败不返回错误:流里偶尔出现半个包或厂商自定义字段,不该把整段对话打断
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return Chunk::Ignore;
    };

    if let Some(err) = error_message(&value) {
        return Chunk::Error(err);
    }

    // choices[0].delta.content —— 流式;非流式的是 message.content,这里也认,
    // 因为有的服务无视 stream:true 直接返回完整响应
    let choices = value.get("choices").and_then(|c| c.as_array());
    let Some(choice) = choices.and_then(|c| c.first()) else {
        return Chunk::Ignore;
    };
    let delta = choice.get("delta").or_else(|| choice.get("message"));
    let Some(text) = delta
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
    else {
        // 只有 role 的首包、只有 reasoning_content 的推理包,都走这儿
        return Chunk::Ignore;
    };
    Chunk::Text(text.to_string())
}

/// 服务端错误对象的文案。OpenAI 是 `{"error":{"message":...}}`,
/// 也有实现直接给 `{"error":"..."}` —— 两种都认。
fn error_message(value: &serde_json::Value) -> Option<String> {
    let err = value.get("error")?;
    if let Some(msg) = err.get("message").and_then(|m| m.as_str()) {
        return Some(msg.to_string());
    }
    if let Some(msg) = err.as_str() {
        return Some(msg.to_string());
    }
    Some(err.to_string())
}

/// 非流式响应(或者压根不是 SSE 的普通响应)里取出正文。
///
/// 用在两个地方:① 服务端不认 `stream` 直接返回完整 JSON;
/// ② 拿它解析错误响应体,好把「model not found」「invalid api key」这类原文显示出来。
pub fn parse_full_response(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("响应不是 JSON: {e}"))?;

    if let Some(err) = error_message(&value) {
        return Err(err);
    }
    let text = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return Err(format!("响应里没有正文: {}", truncate(body, 200)));
    }
    Ok(text.to_string())
}

/// 取响应头里的 `content-type` 判断是不是 SSE。
pub fn is_event_stream(content_type: &str) -> bool {
    content_type.to_ascii_lowercase().contains("text/event-stream")
}

/// 出错时提示里用的截断(别把整个响应体塞进界面)
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 系统提示词。给桌宠一个身份,顺带把「现在有几条没做完、今天几号」告诉它 ——
/// 不用工具调用,它也能答「我今天还有多少事」。
pub fn system_prompt(pending: usize, today: &str) -> String {
    format!(
        "你是「待办清单」应用里住在桌面上的桌宠,名字叫小助手。\
         说话简短、口语化,一次别超过三句话。\
         可以用 Markdown(标题、列表、行内代码、代码块、引用、表格都能渲染),\
         但别为了排版而排版 —— 一两句话的回复直接说就行。\
         现在是 {today},用户还有 {pending} 条任务没完成。\
         用户要你帮忙记事时,提醒他用应用本身的输入框,你不能直接改任务列表。"
    )
}

/// 把底层错误翻成人话。
///
/// 原始错误长这样:`io error: failed to lookup address information`、
/// `tls error: ... certificate verify failed`、`HTTP 401: {"error":...}` ——
/// 直接显示在界面上等于没说。这里按特征归几类,给一句能照做的提示,
/// 后面再把原文缀上(不丢信息,只是前面先给人话)。
pub fn friendly_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let hint = if lower.contains("certificate") || lower.contains("tls") {
        "证书校验没过 —— 如果是自建服务,可能要让它用受信任的证书"
    } else if lower.contains("failed to lookup")
        || lower.contains("dns")
        || lower.contains("connect")
        || lower.contains("connection refused")
        || lower.contains("unreachable")
    {
        "连不上这个地址 —— 检查接口地址写对没、服务在不在跑、要不要挂代理"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "等超时了 —— 服务没响应,稍后再试"
    } else if lower.contains("401") || lower.contains("403")
        || lower.contains("invalid api key") || lower.contains("unauthorized")
        || lower.contains("incorrect api key")
    {
        "密钥不对或没权限 —— 检查密钥有没有过期/写错,以及这个模型你有没有开通"
    } else if lower.contains("429") || lower.contains("quota") || lower.contains("rate limit")
        || lower.contains("insufficient")
    {
        "被限流或者额度用完了 —— 过一会儿再试"
    } else if lower.contains("model") && (lower.contains("not found") || lower.contains("does not exist")) {
        "模型名不对 —— 检查模型名是不是这个服务商支持的那个"
    } else {
        "请求失败"
    };
    format!("{hint}。\n{raw}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_fills_in_the_path() {
        assert_eq!(
            endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        // 尾部斜杠
        assert_eq!(
            endpoint("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        // 前后空白
        assert_eq!(
            endpoint("  http://localhost:11434/v1  "),
            "http://localhost:11434/v1/chat/completions"
        );
        // 已经写全了就别再补
        assert_eq!(
            endpoint("https://x/v1/chat/completions"),
            "https://x/v1/chat/completions"
        );
        assert_eq!(endpoint("https://x/chat/completions"), "https://x/chat/completions");
        // 不擅自补 /v1
        assert_eq!(endpoint("https://x"), "https://x/chat/completions");
    }

    #[test]
    fn body_has_the_expected_shape() {
        let msgs = vec![
            Message { role: Role::System, content: "sys".into() },
            Message { role: Role::User, content: "你好".into() },
        ];
        let body = request_body("deepseek-chat", &msgs, true);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "你好");
    }

    #[test]
    fn parses_streaming_deltas() {
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"content":"你"}}]}"#),
            Chunk::Text("你".into())
        );
        // 冒号后没空格
        assert_eq!(
            parse_sse_line(r#"data:{"choices":[{"delta":{"content":"a"}}]}"#),
            Chunk::Text("a".into())
        );
        // 带 CRLF 和尾部空白
        assert_eq!(
            parse_sse_line("data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\r\n"),
            Chunk::Text("b".into())
        );
    }

    #[test]
    fn ignores_noise_lines() {
        // 心跳注释、空行、event: 字段
        assert_eq!(parse_sse_line(": keep-alive"), Chunk::Ignore);
        assert_eq!(parse_sse_line(""), Chunk::Ignore);
        assert_eq!(parse_sse_line("event: message"), Chunk::Ignore);
        assert_eq!(parse_sse_line("data:"), Chunk::Ignore);
        // 只有 role 的首包
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            Chunk::Ignore
        );
        // 只有推理内容的包(DeepSeek R1 那种):不当作正文,但也别报错
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning_content":"嗯…"}}]}"#),
            Chunk::Ignore
        );
        // 空字符串的 content 也算没内容
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"content":""}}]}"#),
            Chunk::Ignore
        );
    }

    #[test]
    fn recognises_done_and_errors() {
        assert_eq!(parse_sse_line("data: [DONE]"), Chunk::Done);
        assert_eq!(
            parse_sse_line(r#"data: {"error":{"message":"invalid api key"}}"#),
            Chunk::Error("invalid api key".into())
        );
    }

    #[test]
    fn full_response_parses_both_shapes() {
        // 非流式 / 服务端无视 stream:true
        let full = r#"{"choices":[{"message":{"role":"assistant","content":"结果是 42"}}]}"#;
        assert_eq!(parse_full_response(full).unwrap(), "结果是 42");
        // 错误响应
        let err = r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#;
        assert_eq!(parse_full_response(err).unwrap_err(), "model not found");
        let err2 = r#"{"error":"quota exceeded"}"#;
        assert_eq!(parse_full_response(err2).unwrap_err(), "quota exceeded");
        // 不是 JSON(比如网关返回的 HTML)也不能崩
        assert!(parse_full_response("<html>502</html>").is_err());
    }

    #[test]
    fn detects_event_stream() {
        assert!(is_event_stream("text/event-stream"));
        assert!(is_event_stream("text/event-stream; charset=utf-8"));
        assert!(!is_event_stream("application/json"));
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        // 中文按字符截,不能按字节切一半
        assert_eq!(truncate("你好世界", 2), "你好…");
        assert_eq!(truncate("你好", 5), "你好");
    }

    #[test]
    fn friendly_error_gives_actionable_hints() {
        // 每类都要给出「能照做」的那句话,而不是把原始错误原样甩出去
        assert!(friendly_error("io error: failed to lookup address information")
            .contains("连不上"));
        assert!(friendly_error("tls error: certificate verify failed").contains("证书"));
        assert!(friendly_error("HTTP 401: Incorrect API key provided").contains("密钥"));
        assert!(friendly_error("HTTP 429: rate limit exceeded").contains("限流"));
        assert!(friendly_error("HTTP 404: model gpt-9 not found").contains("模型名"));
        // 认不出来的也要有兜底,并且**保留原文**(排查时那才是关键信息)
        let other = friendly_error("something weird happened");
        assert!(other.contains("请求失败"));
        assert!(other.contains("something weird happened"));
    }

    #[test]
    fn system_prompt_mentions_state() {
        let p = system_prompt(3, "2026-09-20");
        assert!(p.contains("2026-09-20"));
        assert!(p.contains('3'));
    }
}
