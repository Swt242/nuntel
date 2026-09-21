#!/usr/bin/env python3
"""一个只会说 OpenAI 协议的假服务,用来端到端验证流式对话。

    python tmp/mock_openai.py [端口]

故意做的一件事:**把 SSE 的字节切成 3 字节一片往外写**,中间还带停顿。
真实的网络分块就是这样,不会对齐到换行 —— 参考项目里直接 `split("\\n\\n")`
就是栽在这上面。我们的实现用 BufReader::lines(),应该照样能拼对。
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

# Windows 上 stdout 重定向到文件时默认是 GBK,打中文会 UnicodeEncodeError 把服务打崩
# (第一次跑就这么崩的)。强制 UTF-8 + 出错也别 raise。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8765

# 故意回一段 Markdown:标题 / 列表 / 行内样式 / 代码块 / 引用 / 表格都有,
# 而且是**拆成小片流式发**的 —— 每一片都可能把某个块切成半截(比如代码围栏只到一半),
# 用来验证「边流边渲染」不会崩、也不会把半截内容吞掉。
REPLY = [
    "## 今天的待办\n\n",
    "你还有 **1 条** 没做完:\n\n",
    "- 推进 Ai 终端项目 —— `9 月 18 日`\n\n",
    "```rust\n",
    "fn main() {\n",
    '    println!("hi");   // *这里的星号不该变斜体*\n',
    "}\n",
    "```\n\n",
    "> 要我先帮你排个顺序吗?\n\n",
    "| 建议 | 原因 |\n",
    "|---|---|\n",
    "| 先做这个 | 今天到期 |\n",
]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"  # 用「连接关闭」当结束标志,省得自己写 chunked

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except Exception as err:  # noqa: BLE001
            print("请求体不是 JSON:", err, flush=True)
            self.send_error(400)
            return

        auth = self.headers.get("Authorization", "")
        api_key = self.headers.get("api-key", "")
        print(
            f"[mock] {self.path} model={body.get('model')!r} "
            f"messages={len(body.get('messages', []))} stream={body.get('stream')} "
            f"auth={'Bearer' if auth.startswith('Bearer ') else 'NONE'} api-key={'yes' if api_key else 'no'}",
            flush=True,
        )
        for m in body.get("messages", []):
            print(f"        {m.get('role'):9} {str(m.get('content'))[:60]!r}", flush=True)

        # 故意把 key 校验做成会失败的样子,方便测错误分支:
        # 传 x-bad-key 就走错误响应
        if api_key == "bad":
            payload = json.dumps({"error": {"message": "Incorrect API key provided"}}).encode()
            self.send_response(401)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream; charset=utf-8")
        self.send_header("cache-control", "no-cache")
        self.end_headers()

        def write_fragmented(text: str):
            """按 3 字节一片写出去,模拟分块不对齐行边界"""
            data = text.encode("utf-8")
            for i in range(0, len(data), 3):
                self.wfile.write(data[i : i + 3])
                self.wfile.flush()
                time.sleep(0.002)

        # 首包只有 role(要能被忽略,不能当成正文)
        write_fragmented('data: {"choices":[{"delta":{"role":"assistant"},"index":0}]}\n\n')
        for piece in REPLY:
            ev = json.dumps(
                {"choices": [{"delta": {"content": piece}, "index": 0}]}, ensure_ascii=False
            )
            write_fragmented(f"data: {ev}\n\n")
            time.sleep(0.08)
        # 中间塞一个只有 reasoning_content 的包(DeepSeek R1 那种):不该混进正文
        write_fragmented('data: {"choices":[{"delta":{"reasoning_content":"(内心戏)"}}]}\n\n')
        # 再塞一行注释(有的服务拿它当心跳)
        write_fragmented(": keep-alive\n\n")
        write_fragmented("data: [DONE]\n\n")

    def log_message(self, *_args):
        pass  # 默认的访问日志太吵,上面已经打够了


if __name__ == "__main__":
    print(f"[mock] 监听 http://127.0.0.1:{PORT}/v1/chat/completions", flush=True)
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
