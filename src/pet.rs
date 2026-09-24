//! 桌宠:动画帧的加载与推进。
//!
//! 这里**只放不依赖 `State` 的部分** —— 窗口的创建、和任务的联动都在 `main.rs`,
//! 因为那些要跟 `State` 互相引用。这样切出来的好处是动画推进的逻辑是纯函数,能单测。
//!
//! 素材从哪来、为什么是逐帧 PNG,见 `tools/pet-assets/src/main.rs` 的说明。

/// 一个动画:名字 + 每帧毫秒 + 帧图片数据。
///
/// `frames` 里是 `include_bytes!` 的 PNG 原始字节 —— 由 `build.rs` 从
/// `assets/pet/manifest.json` 生成,加动画不用改 Rust 代码。
pub struct Animation {
    pub name: &'static str,
    pub delay_ms: u64,
    pub frames: &'static [&'static [u8]],
}

include!(concat!(env!("OUT_DIR"), "/pet_frames.rs"));

/// 解码好的素材。每个动画一组帧图,整个进程只解一次。
pub struct PetFrames {
    /// (每帧毫秒, 帧图)
    anims: Vec<(u64, Vec<slint::Image>)>,
    /// 左右镜像过的帧。**用到哪个动画才现做**(见 `frame_mirrored`)
    mirrored: std::cell::RefCell<std::collections::HashMap<usize, Vec<slint::Image>>>,
}

impl PetFrames {
    /// 把 build.rs 嵌进来的 PNG 全解码成 `slint::Image`。
    ///
    /// 一次解完而不是用时再解:总共十来兆,一次性解掉换来的是切动画时零延迟
    /// (切动画要立刻出画,不能当场解 PNG 卡一下)。
    pub fn load() -> Self {
        let mut anims = Vec::with_capacity(ANIMATIONS.len());
        for anim in ANIMATIONS {
            let frames: Vec<slint::Image> = anim
                .frames
                .iter()
                .filter_map(|bytes| slint::Image::load_from_data(bytes, Some("png")).ok())
                .collect();
            if frames.is_empty() {
                continue;
            }
            anims.push((anim.delay_ms, frames));
        }
        Self { anims, mirrored: Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.anims.is_empty()
    }

    /// 所有动画的名字,按素材清单里的顺序。
    ///
    /// 用途:设置窗口要列出「每个动画」,以及待机轮播要挑一个。
    /// 名字来自 `manifest.json`(build.rs 生成),所以加动画不用改 Rust 代码。
    pub fn names(&self) -> Vec<&'static str> {
        ANIMATIONS
            .iter()
            .take(self.anims.len())
            .map(|a| a.name)
            .collect()
    }

    /// 待机动画的名字(`idle-` 开头的那批)。轮播只在这批里挑 ——
    /// 看书/购物是交互专用的,不该没事自己播。
    pub fn idle_names(&self) -> Vec<&'static str> {
        self.names().into_iter().filter(|n| n.starts_with("idle-")).collect()
    }

    /// 名字 → 下标(找不到给 None)
    pub fn index_of(&self, name: &str) -> Option<usize> {
        ANIMATIONS
            .iter()
            .position(|a| a.name == name)
            .filter(|i| *i < self.anims.len())
    }

    pub fn delay_ms(&self, anim: usize) -> u64 {
        self.anims.get(anim).map(|(d, _)| *d).unwrap_or(200)
    }

    pub fn frame_count(&self, anim: usize) -> usize {
        self.anims.get(anim).map(|(_, f)| f.len()).unwrap_or(0)
    }

    pub fn frame(&self, anim: usize, frame: usize) -> Option<slint::Image> {
        let (_, frames) = self.anims.get(anim)?;
        frames.get(frame % frames.len().max(1)).cloned()
    }

    /// 同一个动画的**左右镜像版**那一帧(往右走的时候用,见 `State::pet_flip`)。
    ///
    /// 为什么在像素层翻、不用 Slint 的 `transform-scale-x: -1`:那套变换在**软件
    /// 渲染器上不生效**(官方文档原话「Transforms are not available for the software
    /// renderer」),而桌宠要透明就只能走软件渲染那条路 —— 写上去是一行,但那一行
    /// 什么都不会发生。
    ///
    /// 代价可以接受:第一次用到某个动画时才现做(5~8 帧 × 224×224×4 ≈ 1.6MB),
    /// 做完缓存住;走路本来就是低频事件。
    pub fn frame_mirrored(&self, anim: usize, frame: usize) -> Option<slint::Image> {
        if let Some(frames) = self.mirrored.borrow().get(&anim) {
            return frames.get(frame % frames.len().max(1)).cloned();
        }
        let (_, src) = self.anims.get(anim)?;
        let flipped: Vec<slint::Image> = src.iter().filter_map(mirror_image).collect();
        if flipped.is_empty() {
            return None; // 一张都翻不出来:让调用方退回原图,别画成空白
        }
        let out = flipped.get(frame % flipped.len()).cloned();
        self.mirrored.borrow_mut().insert(anim, flipped);
        out
    }
}

/// 把一张图左右镜像。拿不到像素就给 `None`(调用方退回原图)。
///
/// `to_rgba8` / `from_rgba8` 都是**非预乘**的,配一对用不会有半透明边的问题
/// (换成 `to_rgba8_premultiplied` 就会,边缘会发暗)。
fn mirror_image(img: &slint::Image) -> Option<slint::Image> {
    let src = img.to_rgba8()?;
    let (w, h) = (src.width(), src.height());
    let mut out = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let mirrored = mirror_rows(src.as_bytes(), w, h);
    // `make_mut_slice` 给的是 `&mut [Rgba8Pixel]`(没有按字节的可变视图),
    // 所以逐像素搬 —— 一次也就 5 万像素,走路这种低频事件看不出来
    for (i, px) in out.make_mut_slice().iter_mut().enumerate() {
        let s = &mirrored[i * 4..i * 4 + 4];
        *px = slint::Rgba8Pixel { r: s[0], g: s[1], b: s[2], a: s[3] };
    }
    Some(slint::Image::from_rgba8(out))
}

/// 把一块**行优先、每像素 4 字节**的像素左右翻转(纯函数,可单测)。
///
/// 行不动、每行内部倒过来 —— 表里像素是 `x` 最快的那一维,所以就是每 4 字节一组
/// 倒着抄一遍。
pub fn mirror_rows(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let stride = w * 4;
    // 数据对不上就原样返回:宁可画一张没翻的图,也不能 panic 或者越界读
    if w == 0 || h == 0 || src.len() < stride * h {
        return src.to_vec();
    }
    let mut out = vec![0u8; stride * h];
    for y in 0..h {
        let row = &src[y * stride..(y + 1) * stride];
        let dst = &mut out[y * stride..(y + 1) * stride];
        for x in 0..w {
            dst[(w - 1 - x) * 4..(w - 1 - x) * 4 + 4].copy_from_slice(&row[x * 4..x * 4 + 4]);
        }
    }
    out
}

// ── 播放顺序与开关(纯逻辑,可单测) ────────────────────────────────────
//
// 设置窗口里那一列动画**可以拖**,而且顺序同时决定待机轮播的顺序(见 §39)。
// 存档里存的是「用户排过的顺序」和「关掉了哪几个」,而素材清单(`manifest.json`)
// 随时可能多一个少一个 —— 两边对不上的各种情况全在这三个函数里消化掉,
// 别让 `State` 里到处写 if。

/// 把「存档里记的顺序」和「素材里实际有的名字」对上,得到真正要用的顺序。
///
/// - 存档顺序为准;
/// - **存档里没有的名字**(新加的素材)按素材清单的顺序补在后面 —— 丢个新素材进去
///   不用改存档就能用;
/// - 存档里有、素材里已经没有的(删了素材)直接丢掉。
pub fn effective_order<'a>(stored: &[String], available: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = Vec::with_capacity(available.len());
    for name in stored {
        if let Some(hit) = available.iter().find(|n| *n == name)
            && !out.contains(hit)
        {
            out.push(hit);
        }
    }
    for name in available {
        if !out.contains(name) {
            out.push(name);
        }
    }
    out
}

/// 把 `name` 挪到第 `to` 位(越界夹到两端;名字不在名单里就原样返回)。
///
/// 语义是「先抽出来再插进去」,所以 `to` 按**原来的**下标给就行 ——
/// UI 那边算出来的落点下标用的就是原列表的坐标系。
pub fn move_in_order(order: &[String], name: &str, to: usize) -> Vec<String> {
    let mut out = order.to_vec();
    let Some(from) = out.iter().position(|n| n == name) else { return out };
    let item = out.remove(from);
    let to = to.min(out.len());
    out.insert(to, item);
    out
}

/// 池子里「当前这个的下一个」,到末尾绕回开头。
///
/// `current` 不在池子里(刚被关掉、或者当前播的是别的动画)就从池首开始;
/// 池子是空的给 `None` —— 调用方据此决定「什么都别做」。
pub fn next_in_cycle<'a>(pool: &[&'a str], current: Option<&str>) -> Option<&'a str> {
    if pool.is_empty() {
        return None;
    }
    match current.and_then(|c| pool.iter().position(|n| *n == c)) {
        Some(i) => Some(pool[(i + 1) % pool.len()]),
        None => Some(pool[0]),
    }
}

// ── 跟随鼠标:走过去那条路(纯逻辑,可单测) ──────────────────────────────
//
// 需求原话是「非直线、较缓慢」。所以:走一条**二次贝塞尔**(控制点从中点往垂直方向
// 拱出去),时间上再走一道 smoothstep(两端慢、中间快)。这里只管算,不碰窗口。

/// 基准步速(物理像素/秒)。实际速度 = 这个 × 设置里的倍率。
const WALK_BASE_SPEED: f32 = 220.0;
/// 走多快都别短于/长于这个 —— 太短像瞬移,太长像卡住
const WALK_MIN_SECS: f32 = 1.2;
const WALK_MAX_SECS: f32 = 8.0;

/// 一次「走过去」的路线
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Walk {
    /// 出发时的宠物中心(物理像素)
    pub from: (f32, f32),
    /// 贝塞尔的控制点
    pub ctrl: (f32, f32),
    /// 目标(宠物中心,物理像素)
    pub to: (f32, f32),
    /// 全程时长(秒)
    pub secs: f32,
}

/// 排一条从 `from` 到 `to` 的路线。
///
/// - **控制点**:取两点中点,再往**垂直于连线**的方向拱出去一段 ——
///   拱的距离是距离的 18%,夹在 40~160px;往哪边拱由 `seed` 的奇偶决定(左右交替,
///   不至于每次都往同一边绕)。
/// - **时长**:按 `speed`(倍率,1 = 基准)算,夹在 1.2~8 秒。
pub fn walk_curve(from: (f32, f32), to: (f32, f32), speed: f32, seed: u64) -> Walk {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = (dx * dx + dy * dy).sqrt();
    let mid = ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);

    // 垂直方向(把连线转 90°);两点重合时没有方向,随便给一个
    let (nx, ny) = if dist > 0.5 { (-dy / dist, dx / dist) } else { (0.0, -1.0) };
    let bow = (dist * 0.18).clamp(40.0, 160.0) * if seed % 2 == 0 { 1.0 } else { -1.0 };
    let ctrl = (mid.0 + nx * bow, mid.1 + ny * bow);

    let speed = speed.max(0.05);
    let secs = (dist / (WALK_BASE_SPEED * speed)).clamp(WALK_MIN_SECS, WALK_MAX_SECS);
    Walk { from, ctrl, to, secs }
}

/// 路上第 `t`(0..1)时刻的位置。`t` 会先过一道 smoothstep —— 起步和停下都慢,
/// 这就是「较缓慢」的手感来源。
pub fn curve_point(walk: &Walk, t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let t = t * t * (3.0 - 2.0 * t); // smoothstep
    let u = 1.0 - t;
    let (a, b, c) = (u * u, 2.0 * u * t, t * t);
    (
        a * walk.from.0 + b * walk.ctrl.0 + c * walk.to.0,
        a * walk.from.1 + b * walk.ctrl.1 + c * walk.to.1,
    )
}

/// 当前正在播的这个动画允不允许触发跟随。
///
/// `off` 里是「不允许跟随」的动画名 —— **缺省 = 允许**(新素材进来默认能跟随),
/// 和 `pet_anim_off` 是同一个套路。
pub fn follow_allowed(off: &std::collections::HashSet<String>, current: &str) -> bool {
    !off.contains(current)
}

/// 动画推进:把这段时间累加上去,够了就翻一帧。
///
/// 返回 `(新的帧下标, 剩余累积时间)`。
///
/// 为什么不用「定时器间隔 = 帧时长」:每帧的时长不一样(见 manifest.json),
/// 那样得每次重启定时器、还要重新注册回调。固定一个细粒度的心跳、自己累积,
/// 反而简单且不怕掉帧 —— 卡了一下就一次多翻几帧,而不是整体变慢。
///
/// `left_ms` 是上一次剩下的累积时间。**用 `while` 而不是 `if`**:
/// 掉帧时一次 tick 可能够翻好几帧(比如窗口被拖动时系统在忙),用 if 会让动画变慢。
pub fn advance(
    frame: usize,
    count: usize,
    mut left_ms: f64,
    dt_ms: f64,
    delay_ms: u64,
) -> (usize, f64) {
    if count == 0 {
        return (0, 0.0);
    }
    let delay = delay_ms.max(1) as f64;
    left_ms += dt_ms;
    let mut f = frame % count;
    while left_ms >= delay {
        left_ms -= delay;
        f = (f + 1) % count;
    }
    (f, left_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 累积够了才翻帧,并且把余数留下
    #[test]
    fn advances_only_when_enough_time_passed() {
        let (f, left) = advance(0, 8, 0.0, 100.0, 250);
        assert_eq!(f, 0, "100ms 还不够 250ms 的一帧");
        assert!((left - 100.0).abs() < 0.01);

        let (f, left) = advance(0, 8, 100.0, 200.0, 250);
        assert_eq!(f, 1, "攒到 300ms 应当翻一帧");
        assert!((left - 50.0).abs() < 0.01, "余下的 50ms 要留住,不能清零");
    }

    /// **掉帧时一次 tick 可能够翻好几帧** —— 用 if 的话动画会整体变慢
    #[test]
    fn catches_up_after_a_stall() {
        // 卡了 1 秒,帧长 250ms → 应当一次翻 4 帧
        let (f, left) = advance(0, 8, 0.0, 1000.0, 250);
        assert_eq!(f, 4, "卡顿之后要一次追上,而不是慢慢补");
        assert!(left < 250.0);
    }

    /// 帧下标要循环,不能越界
    #[test]
    fn wraps_around() {
        let (f, _) = advance(7, 8, 0.0, 250.0, 250);
        assert_eq!(f, 0, "最后一帧之后回到第 0 帧");
    }

    /// 帧数为 0 时不能除零/panic
    #[test]
    fn empty_animation_is_safe() {
        let (f, left) = advance(3, 0, 10.0, 100.0, 200);
        assert_eq!(f, 0);
        assert_eq!(left, 0.0);
    }

    // ── 顺序与开关 ────────────────────────────────────────────────────

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// 存档里没排过序(空)→ 就是素材清单的顺序
    #[test]
    fn empty_stored_order_means_manifest_order() {
        let available = ["idle-1", "read", "idle-2"];
        assert_eq!(effective_order(&[], &available), available.to_vec());
    }

    /// 存档顺序优先;新加的素材补在后面,删掉的素材直接丢
    #[test]
    fn stored_order_wins_and_new_assets_go_last() {
        let available = ["idle-1", "idle-2", "idle-3", "read"];
        assert_eq!(
            effective_order(&owned(&["read", "idle-2", "shop", "idle-1"]), &available),
            vec!["read", "idle-2", "idle-1", "idle-3"],
            "shop 素材没了要丢掉,idle-3 是新的补在末尾"
        );
    }

    /// 挪位置:抽出来再插进去,边界要夹住
    #[test]
    fn move_in_order_handles_edges() {
        let order = owned(&["a", "b", "c", "d"]);
        assert_eq!(move_in_order(&order, "a", 2), owned(&["b", "c", "a", "d"]));
        assert_eq!(move_in_order(&order, "d", 0), owned(&["d", "a", "b", "c"]));
        // 原地不动
        assert_eq!(move_in_order(&order, "b", 1), order);
        // 越界夹到末尾
        assert_eq!(move_in_order(&order, "a", 99), owned(&["b", "c", "d", "a"]));
        // 名字不在名单里:原样返回
        assert_eq!(move_in_order(&order, "zzz", 0), order);
    }

    // ── 跟走的路线 ────────────────────────────────────────────────────

    /// 曲线两端必须正好落在起点和终点(不然宠物会走偏)
    #[test]
    fn curve_hits_both_ends() {
        let w = walk_curve((100.0, 100.0), (500.0, 300.0), 1.0, 0);
        assert_eq!(curve_point(&w, 0.0), (100.0, 100.0));
        let end = curve_point(&w, 1.0);
        assert!((end.0 - 500.0).abs() < 0.01 && (end.1 - 300.0).abs() < 0.01, "{end:?}");
    }

    /// **不是直线**:中点要明显偏离两点连线
    #[test]
    fn curve_bows_off_the_straight_line() {
        let (from, to) = ((100.0f32, 100.0f32), (500.0, 100.0));
        let w = walk_curve(from, to, 1.0, 0);
        let mid = curve_point(&w, 0.5);
        let straight_y = (from.1 + to.1) / 2.0;
        assert!(
            (mid.1 - straight_y).abs() > 20.0,
            "中点 {mid:?} 几乎在直线上(直线 y={straight_y})"
        );
    }

    /// 拱出去的方向:seed 奇偶各走一边
    #[test]
    fn curve_bows_to_both_sides() {
        let (from, to) = ((0.0f32, 0.0f32), (400.0, 0.0));
        let a = curve_point(&walk_curve(from, to, 1.0, 0), 0.5);
        let b = curve_point(&walk_curve(from, to, 1.0, 1), 0.5);
        assert!(a.1 * b.1 < 0.0, "两次应该拱向相反的两侧:{a:?} / {b:?}");
    }

    /// 拱出量随距离变化,但被夹在 40~160
    #[test]
    fn curve_bow_is_clamped() {
        // 很近:按 18% 只该拱 3.6px,夹到 40
        let near = walk_curve((0.0, 0.0), (10.0, 0.0), 1.0, 0);
        let d_near = (near.ctrl.1 - 0.0).abs();
        assert!((d_near - 40.0).abs() < 0.01, "近距应该夹到 40,实得 {d_near}");
        // 很远:18% 会超过 160,也夹住
        let far = walk_curve((0.0, 0.0), (3000.0, 0.0), 1.0, 0);
        let d_far = (far.ctrl.1 - 0.0).abs();
        assert!((d_far - 160.0).abs() < 0.01, "远距应该夹到 160,实得 {d_far}");
    }

    /// 时长:速度倍率越大越短,并且两头都夹住
    #[test]
    fn walk_duration_follows_speed_and_is_clamped() {
        let slow = walk_curve((0.0, 0.0), (880.0, 0.0), 0.5, 0).secs;
        let fast = walk_curve((0.0, 0.0), (880.0, 0.0), 2.0, 0).secs;
        assert!(slow > fast, "慢的应该更久:{slow} vs {fast}");
        // 880px / 220 = 4 秒(1×)
        let one = walk_curve((0.0, 0.0), (880.0, 0.0), 1.0, 0).secs;
        assert!((one - 4.0).abs() < 0.01, "1× 走 880px 应该是 4 秒,实得 {one}");
        // 太近不会瞬移,太远不会走到天荒地老
        assert_eq!(walk_curve((0.0, 0.0), (5.0, 0.0), 1.0, 0).secs, WALK_MIN_SECS);
        assert_eq!(walk_curve((0.0, 0.0), (99999.0, 0.0), 0.25, 0).secs, WALK_MAX_SECS);
    }

    // ── 左右镜像(往右走时用的帧) ─────────────────────────────────────

    /// 每行倒过来,行与行之间不动
    #[test]
    fn mirror_rows_flips_each_row() {
        // 2×2,一个像素 4 字节;第 0 列全是 1、第 1 列全是 2,好认
        let src = vec![
            1, 1, 1, 1, 2, 2, 2, 2, //
            3, 3, 3, 3, 4, 4, 4, 4,
        ];
        assert_eq!(
            mirror_rows(&src, 2, 2),
            vec![
                2, 2, 2, 2, 1, 1, 1, 1, //
                4, 4, 4, 4, 3, 3, 3, 3,
            ]
        );
    }

    /// 翻两次回到原样;数据长度对不上就原样返回(不 panic)
    #[test]
    fn mirror_rows_is_its_own_inverse() {
        let src: Vec<u8> = (0..(3 * 2 * 4) as u8).collect();
        assert_eq!(mirror_rows(&mirror_rows(&src, 3, 2), 3, 2), src);
        assert_eq!(mirror_rows(&[7, 7, 7], 3, 2), vec![7, 7, 7], "长度不够原样返回");
        assert_eq!(mirror_rows(&[], 0, 0), Vec::<u8>::new(), "空图不能炸");
    }

    /// 透明像素的 4 个字节(含 alpha)一起搬,不能只翻 RGB
    #[test]
    fn mirror_rows_keeps_alpha_in_place() {
        let src = vec![10, 20, 30, 40, 50, 60, 70, 80];
        assert_eq!(mirror_rows(&src, 2, 1), vec![50, 60, 70, 80, 10, 20, 30, 40]);
    }

    /// 跟随判定:缺省允许,记进 off 的才不允许
    #[test]
    fn follow_is_allowed_unless_marked_off() {
        let mut off = std::collections::HashSet::new();
        assert!(follow_allowed(&off, "idle-1"));
        off.insert("idle-1".to_string());
        assert!(!follow_allowed(&off, "idle-1"));
        assert!(follow_allowed(&off, "shop"));
    }

    /// 轮播取「下一个」:末尾绕回开头;当前不在池子里就从池首开始;空池子给 None
    #[test]
    fn next_in_cycle_wraps_and_handles_missing_current() {
        let pool = ["a", "b", "c"];
        assert_eq!(next_in_cycle(&pool, Some("a")), Some("b"));
        assert_eq!(next_in_cycle(&pool, Some("c")), Some("a"));
        assert_eq!(next_in_cycle(&pool, Some("zzz")), Some("a"));
        assert_eq!(next_in_cycle(&pool, None), Some("a"));
        assert_eq!(next_in_cycle(&[], Some("a")), None);
        // 只有一个:永远还是它
        assert_eq!(next_in_cycle(&["only"], Some("only")), Some("only"));
    }
}
