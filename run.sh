#!/usr/bin/env sh
# 在本机构建/运行。
#
# 为什么不用默认工具链:这台机器没装 MSVC 链接器(Visual Studio Build Tools)
# 和 Windows SDK,默认的 x86_64-pc-windows-msvc 连 build script 都链接不出来。
# 这里改用 rustup 的 GNU 工具链:自带 rust-lld 和 MinGW 自包含库,不需要管理员。
#
# 装了 VS Build Tools(C++ 工作负载)之后,直接 `cargo run` 即可,不需要本脚本。
#
# 用法:./run.sh build / ./run.sh run / ./run.sh check / ./run.sh run --release
set -e

# Git Bash 里 ~/.cargo/bin 不一定在 PATH 上(rustup 只写进 Windows 的 PATH)
if ! command -v rustup >/dev/null 2>&1; then
    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
fi

GNU_TOOLCHAIN="$HOME/.rustup/toolchains/stable-x86_64-pc-windows-gnu"
SELF_CONTAINED="$GNU_TOOLCHAIN/lib/rustlib/x86_64-pc-windows-gnu/lib/self-contained"
DLLTOOL_DIR="$GNU_TOOLCHAIN/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained"
BINUTILS_DIR="$HOME/.slint-tools/binutils"
MINGW_LIBS="$HOME/.slint-tools/mingw-libs"

# 1) rust-mingw 自带的导入库不全(缺 libshlwapi.a 等,链接最后一步会报
#    "cannot find -lshlwapi")。从 MSYS2 的 crt 包里补齐,只补自包含库里
#    没有的,不覆盖原文件。rustup 重装工具链后这里会自动补回来。
STAMP="$SELF_CONTAINED/.nuntel-mingw-libs"
if [ -d "$MINGW_LIBS" ] && [ -d "$SELF_CONTAINED" ] && [ ! -e "$STAMP" ]; then
    for lib in "$MINGW_LIBS"/lib*.a; do
        name=$(basename "$lib")
        [ -e "$SELF_CONTAINED/$name" ] || cp "$lib" "$SELF_CONTAINED/$name"
    done
    : >"$STAMP"
    echo "已补齐 rust-mingw 缺失的导入库 -> $SELF_CONTAINED"
fi

# 2) windows-* 系列 crate 在 Windows 上固定用 raw-dylib,需要 dlltool 生成
#    导入库;dlltool 又要调 GNU 汇编器 as,这两样 Git for Windows 都不带。
#    注意:rustup 自带的 dlltool 是去**自己所在目录**找 as 的(实测放 PATH 上
#    不管用,报 "dlltool.exe: CreateProcess"),所以要把 binutils 复制到它旁边。
#    只复制它没有的,不覆盖 rustup 自己的 ld.exe / gcc。
BINUTILS_STAMP="$DLLTOOL_DIR/.nuntel-binutils"
if [ -d "$BINUTILS_DIR" ] && [ -d "$DLLTOOL_DIR" ] && [ ! -e "$BINUTILS_STAMP" ]; then
    for tool in "$BINUTILS_DIR"/*.exe; do
        name=$(basename "$tool")
        [ -e "$DLLTOOL_DIR/$name" ] || cp "$tool" "$DLLTOOL_DIR/$name"
    done
    : >"$BINUTILS_STAMP"
    echo "已把 binutils 补到 dlltool 旁边 -> $DLLTOOL_DIR"
fi

if [ -d "$DLLTOOL_DIR" ] && [ -d "$BINUTILS_DIR" ]; then
    PATH="$DLLTOOL_DIR:$BINUTILS_DIR:$PATH"
    export PATH
fi

exec rustup run stable-x86_64-pc-windows-gnu cargo "$@" --target x86_64-pc-windows-gnu
