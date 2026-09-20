#!/usr/bin/env python3
"""给 tools/*.ps1 里含非 ASCII 的文件补上 UTF-8 BOM。

**为什么必须**:没有 BOM 的 .ps1 会被 Windows PowerShell 按 ANSI(GBK)读,
中文注释最后一个字的多字节序列会吃掉行尾换行,把下一行并进注释 ——
表现是 `param()` 里某个变量神秘绑不上值(静默进了 `$args`)。这个坑栽过两次,
而且**用编辑器或脚本重写文件会顺手把 BOM 去掉**,所以改完 .ps1 跑一下这个。

  python tools/ensure-bom.py
"""
import glob

fixed = []
for path in glob.glob('tools/*.ps1'):
    raw = open(path, 'rb').read()
    if raw.startswith(b'\xef\xbb\xbf'):
        continue
    try:
        text = raw.decode('utf-8')
    except UnicodeDecodeError:
        continue
    if any(ord(c) > 127 for c in text):
        open(path, 'wb').write(b'\xef\xbb\xbf' + raw)
        fixed.append(path)

print('补了 BOM:', ', '.join(fixed) if fixed else '(都已经是好的)')
