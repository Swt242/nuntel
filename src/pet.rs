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
        Self { anims }
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
}
