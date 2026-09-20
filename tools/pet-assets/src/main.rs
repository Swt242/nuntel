//! 把桌宠素材的 GIF 拆成逐帧 PNG。
//!
//! 为什么要这一步:
//!
//! 1. **Slint 的 `Image` 是静态的**,不会播放动图 —— 帧得由宿主自己一张张推。
//!    既然要逐帧,不如在构建前就把帧拆好。
//! 2. 原素材是 1024×1024 的 GIF,10 个共 12MB。桌宠实际只显示两百来像素,
//!    缩到 2 倍显示尺寸(PNG + 透明通道)之后仓库里只剩很小一份。
//! 3. GIF 解码要 `image` crate,而 Slint 默认只支持 PNG/JPEG/SVG ——
//!    转成 PNG 之后**主程序一个额外依赖都不用加**。
//!
//! 用法(在仓库根目录):
//!   cargo run --release --manifest-path tools/pet-assets/Cargo.toml
//!
//! 输入:`refs/Angelina/art/images/*.gif`(那个参考项目的素材,不进我们的仓库)
//! 输出:`assets/pet/<名字>/00.png …` + `assets/pet/manifest.json`

use std::fs;
use std::io::BufReader;
use std::path::Path;

use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, RgbaImage};

/// 输出边长。桌宠按 112px 显示,这里出 2 倍给高 DPI 留余量。
const SIZE: u32 = 224;

/// 源文件名 → 输出名。中文名转成 ASCII,免得路径在不同系统上出岔子。
/// 顺序就是「待机时循环播放」的顺序。
const SOURCES: &[(&str, &str)] = &[
    ("1.gif", "idle-1"),
    ("2.gif", "idle-2"),
    ("3.gif", "idle-3"),
    ("4.gif", "idle-4"),
    ("5.gif", "idle-5"),
    ("6.gif", "idle-6"),
    ("7.gif", "idle-7"),
    ("8.gif", "idle-8"),
    ("看书.gif", "read"),
    ("购物.gif", "shop"),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let src_dir = Path::new("refs/Angelina/art/images");
    let out_root = Path::new("assets/pet");
    if !src_dir.is_dir() {
        return Err(format!("找不到素材目录 {} —— 先把参考项目拉到 refs/ 下", src_dir.display()).into());
    }
    fs::create_dir_all(out_root)?;

    let mut manifest = Vec::new();

    for (file, name) in SOURCES {
        let path = src_dir.join(file);
        let decoder = match GifDecoder::new(BufReader::new(fs::File::open(&path)?)) {
            Ok(d) => d,
            Err(err) => {
                eprintln!("跳过 {file}: {err}");
                continue;
            }
        };

        let dir = out_root.join(name);
        fs::create_dir_all(&dir)?;

        let mut count = 0u32;
        // 取第一帧的延迟代表整个动画 —— 这些素材每帧时长基本一致,
        // 逐帧记一份 delay 表没必要。
        let mut delay_ms = 100u32;
        for frame in decoder.into_frames() {
            let frame = frame?;
            if count == 0 {
                let (num, den) = frame.delay().numer_denom_ms();
                if den > 0 && num > 0 {
                    delay_ms = (num / den).clamp(20, 1000);
                }
            }
            let buf = frame.into_buffer();
            let resized = resize_cover(&buf, SIZE);
            resized.save(dir.join(format!("{count:02}.png")))?;
            count += 1;
        }

        println!("{file:10} → assets/pet/{name}/  {count} 帧,每帧 {delay_ms}ms");
        manifest.push((name.to_string(), count, delay_ms));
    }

    // 清单交给宿主读:帧数、每帧毫秒数
    let json: Vec<String> = manifest
        .iter()
        .map(|(n, c, d)| format!(r#"  {{ "name": "{n}", "frames": {c}, "delay": {d} }}"#))
        .collect();
    fs::write(
        out_root.join("manifest.json"),
        format!("[\n{}\n]\n", json.join(",\n")),
    )?;
    println!("\n清单写到 assets/pet/manifest.json");

    Ok(())
}

/// 等比缩放到 SIZE×SIZE 并居中裁掉多余部分。
///
/// 不能直接拉伸:素材是正方形,但万一以后换成非正方形的,拉伸会把人物压扁。
fn resize_cover(src: &RgbaImage, size: u32) -> RgbaImage {
    let (w, h) = (src.width(), src.height());
    let scale = (size as f32 / w as f32).max(size as f32 / h as f32);
    let (nw, nh) = ((w as f32 * scale).round() as u32, (h as f32 * scale).round() as u32);
    let scaled = image::imageops::resize(src, nw.max(1), nh.max(1), FilterType::Lanczos3);
    let x = (nw.saturating_sub(size)) / 2;
    let y = (nh.saturating_sub(size)) / 2;
    image::imageops::crop_imm(&scaled, x, y, size.min(nw), size.min(nh)).to_image()
}
