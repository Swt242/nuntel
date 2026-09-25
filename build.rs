use std::path::PathBuf;
use std::process::Command;

fn main() {
    // 把 ui/app.slint 编译成 Rust 代码,再由 slint::include_modules!() 引入。
    // 加 SLINT_EMIT_DEBUG_INFO=1 编译可以带上元素 id/源码位置,
    // 配合 `cargo run --features slint/mcp` 的运行时 MCP 调试。
    slint_build::compile("ui/app.slint").expect("编译 ui/app.slint 失败");

    embed_windows_icon();
    generate_pet_frames();
}

/// 扫描 `assets/pet/` 下的动画帧,生成一张「帧表」给宿主用。
///
/// 为什么要生成而不是在运行时读文件:桌宠素材必须**跟着 exe 走**。
/// 运行时按相对路径读,一旦从别的目录启动(开机自启、快捷方式)就找不到图了。
/// 用 `include_bytes!` 嵌进来则与工作目录无关。
///
/// 帧数、帧间隔由 `tools/pet-assets` 生成的 `manifest.json` 决定 ——
/// 往 `assets/pet/` 里丢一个新目录 + 一条 manifest,不用改任何 Rust 代码。
fn generate_pet_frames() {
    let root = PathBuf::from("assets/pet");
    println!("cargo:rerun-if-changed=assets/pet/manifest.json");
    if !root.is_dir() {
        // 没素材也能编过,只是没有桌宠动画 —— 别因为缺素材卡住整个构建
        println!("cargo:warning=没有 assets/pet,桌宠不会有动画(先跑 tools/pet-assets)");
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("pet_frames.rs");
        std::fs::write(&out, "pub const ANIMATIONS: &[Animation] = &[];\n").unwrap();
        return;
    }

    let manifest = std::fs::read_to_string(root.join("manifest.json")).unwrap_or_else(|err| {
        panic!("读不了 assets/pet/manifest.json: {err}");
    });
    let items: Vec<serde_json::Value> = serde_json::from_str(&manifest).expect("manifest.json 格式不对");

    let mut code = String::from(
        "// 由 build.rs 生成,别手改 —— 改素材请跑 tools/pet-assets\n\
         pub const ANIMATIONS: &[Animation] = &[\n",
    );
    for item in &items {
        let name = item["name"].as_str().unwrap();
        let frames = item["frames"].as_u64().unwrap();
        let delay = item["delay"].as_u64().unwrap();
        println!("cargo:rerun-if-changed=assets/pet/{name}");
        code.push_str(&format!("    Animation {{ name: \"{name}\", delay_ms: {delay}, frames: &[\n"));
        for i in 0..frames {
            // include_bytes! 的路径相对**本文件**(build.rs),所以从仓库根写起
            code.push_str(&format!(
                "        include_bytes!(\"{}/assets/pet/{name}/{i:02}.png\"),\n",
                env!("CARGO_MANIFEST_DIR").replace('\\', "/")
            ));
        }
        code.push_str("    ] },\n");
    }
    code.push_str("];\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("pet_frames.rs");
    std::fs::write(&out, code).expect("写 pet_frames.rs 失败");
}

/// 把 `assets/icon.ico` 嵌进 exe 的资源段。
///
/// Windows 上文件图标只能来自资源段,不嵌的话桌面/任务栏/资源管理器里显示的是
/// 通用默认图标。资源要用 rc 编译器编成 COFF 目标文件再交给链接器:
/// 优先用 GNU 的 `windres`(GNU 工具链自带),
/// 找不到就跳过 —— 不阻断构建,只是图标退化成默认的。
fn embed_windows_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/icon.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR 应该有"));
    let obj = out_dir.join("icon.o");

    let build = |tool: &str, extra: &[&str]| {
        Command::new(tool)
            .args(extra)
            .args(["assets/icon.rc", "-O", "coff", "-o"])
            .arg(&obj)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };

    // 常规情况:windres 自己会去找 gcc 做预处理。要是环境里没有 C 预处理器
    // (只有 GNU 工具链的 binutils 而没有 gcc),就退一步用 `cat` 假装预处理器 ——
    // 反正 assets/icon.rc 只有一行、没有宏也没有注释(cat 不会剥注释,windres
    // 不认识带注释的原文,所以那个文件里别写注释)。
    let ok = build("windres", &[])
        || build("x86_64-w64-mingw32-windres", &[])
        || build("windres", &["--preprocessor=cat"]);

    if ok {
        println!("cargo:rustc-link-arg-bins={}", obj.display());
        return;
    }

    // MSVC 工具链下没有 windres,用 Windows SDK 里的 rc.exe 生成 .res
    // (link.exe 能直接吃 .res)。我们的 .rc 只有一行 ICON,不需要 SDK 头文件。
    if let Some(rc) = find_windows_sdk_rc() {
        let res = out_dir.join("icon.res");
        let ok = Command::new(rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg("assets/icon.rc")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok {
            println!("cargo:rustc-link-arg-bins={}", res.display());
            return;
        }
    }

    println!("cargo:warning=没找到 windres 或 rc.exe,exe 会使用默认图标");
}

/// 在装了 Windows SDK 的机器上找 rc.exe(取版本号最大的那个 x64 版本)
fn find_windows_sdk_rc() -> Option<PathBuf> {
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.join("x64").join("rc.exe").exists())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join("x64").join("rc.exe"))
}
