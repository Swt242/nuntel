# 第三方内容声明

本仓库的 **代码**以 [MIT 许可证](LICENSE) 授权。但仓库里有若干**不属于代码**的内容,
它们不适用 MIT,情况各不相同,分别写在下面。

---

## 1. 桌宠美术素材(`assets/pet/`)

**版权归鹰角网络(Hypergryph)所有,不适用本仓库的 MIT 许可证。**

- 内容:62 个逐帧 PNG(`idle-1` … `idle-8`、`read`、`shop`)+ `manifest.json`
- 来源:鹰角网络随 2026 年夏活 SideStory「直到大地变成一颗酸橙」/ EP-《酸橙色信笺》
  发布的**官方素材包**
  ([官方发布公告](https://www.skland.com/article?id=6098813),
  [素材包下载](https://link.hypergryph.com/c/20UR6mkR))
- 本仓库里的这些 PNG 由 `tools/pet-assets` 从素材包的 GIF 转出来(拆帧 + 缩到 224×224)

### ⚠️ 使用前请自行核实

**本仓库不对这些素材的授权做任何主张或保证。** 官方发布时的表述是"提供给各位博士
自由使用",但那**不是**一份开源许可证,也没有附带授权条款文件。所以:

- 我们把**代码**以 MIT 授权,**美术素材不在其中** —— 拿这个仓库的代码时,不要
  默认素材也一并授权给你了
- 你要把素材用于自己的项目(尤其是**商业用途**)之前,**请自行向鹰角网络确认**
  当前的使用条款与边界
- 素材的版权与最终解释权都在鹰角网络。如果权利人提出异议,请移除 `assets/pet/`

### 不使用素材也没问题

程序**不依赖**这些素材就能编译运行:

- 删掉整个 `assets/pet/` 目录 → 桌宠自动不启动,其余功能(待办、日历、笔记、AI 对话)全都正常
- 换成自己的图 → 逐帧 PNG 放进 `assets/pet/<名字>/`,在 `manifest.json` 里加一行
  `{ "name": ..., "frames": ..., "delay": ... }`,**不用改任何 Rust 代码**
  (帧表由 `build.rs` 生成,详细步骤见 README 的「桌宠」一节)

---

## 2. 截图(`screenshot-*.png`)

README 里那几张截图是程序界面的实拍,可以按 MIT 使用。但要注意截图里可能**叠进了
桌面壁纸**等第三方内容 —— 壁纸同样归其权利人所有,不在 MIT 范围内。

---

## 3. 依赖项

Cargo 依赖各有各的许可证(Slint、Skia、tree-sitter 及各语法包、chrono、serde …),
都是各自的上游项目授权的,与本仓库的 MIT 无关。它们的许可证文本随依赖分发,
也可以用 [`cargo-license`](https://crates.io/crates/cargo-license) 一次性列出来:

```sh
cargo install cargo-license
cargo license
```

需要留意的是 **Skia**(经 `skia-safe` / `skia-bindings`,仅 `--features skia` 时启用):
它是 BSD 风格的许可证,副本随构建产物落在 `target/*/build/skia-bindings-*/out/skia/LICENSE_SKIA`。

---

## 4. 项目名称与角色

"Nuntel" 是本项目的名字(取自拉丁语 *nuntius*)。仓库与代码里**不包含**任何鹰角网络的
商标、logo 或角色标识 —— 桌宠形象属于上面第 1 条的美术素材,和名称无关。

本项目是**非官方**的个人作品,与鹰角网络(Yostar / Hypergryph)**没有任何隶属或
背书关系**。
