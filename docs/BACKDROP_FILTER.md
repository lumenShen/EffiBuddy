# Backdrop Filter（毛玻璃背景滤镜）

`crates/backdrop`（`effibuddy-backdrop`）实现了 CSS `backdrop-filter` 的核心能力：把面板背后的画面模糊、
调色、叠一层色纱，并按圆角把面板裁出来。它有两个后端（CPU / Vulkan Compute），
共用同一份"计划层"，因此两者的输出可以逐像素比对。

- 设计系统封装：`crates/ui/src/glass.rs`（`effibuddy_ui::glass`）
- 基准程序：`cargo run --release -p effibuddy-backdrop --bin backdrop-bench`
- 本机实测（Ryzen 9 8945HX + RTX 5060 Laptop）：见下表，全部场景 ≥ 300 fps。

## 1. 算法

```
源区域 (region)
   │  ① downsample：n×n 盒式平均，边长缩到 1/n，像素数缩到 1/n²
   ▼
低分辨率图 low  ── ② blur_h：定点高斯（可分离，~11 taps）──► tmp
                    ③ blur_v ──► low'
   │  ④ composite：双线性放大 + 调色矩阵 + 色纱 + 圆角遮罩，一次写出
   ▼
输出 RGBA8（region.width × region.height）
```

关键点是**先把分辨率降下来再模糊**，而不是在原分辨率上跑大核卷积：

- CSS 的 `blur(r)` 等价于 σ = r/2 的高斯，半径 r 的核需要 `2·3σ+1` 个 tap。1080p、
  r=48 时直接算要 ~289 tap/像素。
- 降采样本身就是低通滤波。计划层按 `n = 2^ceil(log2(σ / σ_max))` 选降采样倍数，
  使低分辨率下的 σ ≤ σ_max（Fast 1.5 / Balanced 2.5 / High 4.0），于是**核永远是
  3~25 个 tap，与 CSS 半径无关**。r=48 → n=8，低分辨率图只有 240×135，
  横向+纵向各 11 tap，模糊本身的成本可以忽略。

另外两条同样的原则：

- **圆角遮罩只算一次**。遮罩只依赖 (区域尺寸, 圆角半径, 边缘柔度)，CPU 后端把它缓存成
  一张 u8 平面，逐帧只做一次乘法；GPU 后端在 composite 里算一次 SDF。
- **调色折叠成矩阵**。`brightness → contrast → saturate → tint` 全部是仿射变换，
  每帧只在 CPU 上算一个 3×3 矩阵 + 偏移，内层循环里就是 9 次乘加，没有分支。

`Plan`（`src/plan.rs`）是唯一决定"降采样倍数 / σ / 核"的地方，两个后端都只消费它，
所以对不上时必然是某一边的 bug（`tests/backends.rs` 就是拿这个做回归的）。

## 2. API

```rust
use effibuddy_backdrop::{BackdropFilter, BackdropParams, CornerRadius, ImageRef, Rect};

let mut filter = BackdropFilter::new();       // 有 Vulkan 就用 GPU，否则自动退回 CPU
let mut out = Vec::new();
filter.apply_into_vec(
    &ImageRef::new(&frame, 1920, 1080),
    Rect::new(560, 240, 800, 600),
    &BackdropParams { blur_radius: 24.0, corners: CornerRadius::all(14.0), ..Default::default() },
    &mut out,
)?;
filter.stats();        // 后端、耗时、读写字节数、带宽
filter.backend();      // BackendKind::Cpu | Gpu

// 可选：给滤镜一个帧预算，超了就自动降级到 Quality::Fast，恢复后再切回来
filter.set_frame_budget(Some(Duration::from_secs_f64(1.0 / 300.0)));
filter.is_downgraded();
filter.effective_quality(Quality::High);   // 实际使用的画质
```

降级策略是"黏"的：连续 3 帧超预算才降（避免抖动），连续 120 帧都在预算一半以下才恢复。
本机永远不会触发，但它让"300 fps"这句话在 4K 屏 / 老 iGPU 上也成立。

设计系统侧（`effibuddy_ui::glass`）：

```rust
use effibuddy_ui::glass::{self, GlassSurface};

let mut surface = GlassSurface::new(glass::DIALOG.with_corner(14.0));
let pixels = surface.refresh(ImageRef::new(&wallpaper, w, h), Rect::new(560, 240, 800, 600))?;
let image = glass::to_render_image(pixels, 800, 600);   // 可直接交给 img()
```

`GlassSurface` 自带输出缓冲、统计信息与失效逻辑：`set_style()` 会把缓存标记为脏，
`render_image()` 在脏的时候返回 `None`。预设：`DIALOG / DIALOG_DARK / MENU / PANE`。

## 3. 性能

`cargo run --release -p effibuddy-backdrop --bin backdrop-bench`（240 次取样，取最优值）：

| 场景 | GPU 后端 | CPU 后端 |
| --- | --- | --- |
| 菜单 320×240, r=18 | **0.11 ms / 9200 fps** | 0.15 ms / 6800 fps |
| 对话框 800×600, r=24 | **0.45 ms / 2250 fps** | 0.48 ms / 2050 fps |
| 侧栏 380×1080, r=32 | 0.39 ms / 2600 fps | 0.38 ms / 2600 fps |
| 全窗 1920×1080, r=24 | 1.80 ms / 555 fps | 1.80 ms / 550 fps |
| 全窗 1920×1080, r=64 | 1.88 ms / 530 fps | **1.52 ms / 660 fps** |

> 3 次取样（每次 400 帧）的范围，抖动 < 5%。目标 300 fps（3.33 ms/帧），
> 所有场景都有 1.7 倍以上余量。
>
> 注意 r=64 的全窗场景 GPU 反而更慢：模糊半径越大降采样越狠（n=16），
> 低分辨率图只剩 120×68，CPU 的模糊+合成几乎免费，而 GPU 仍要付
> "上传 8 MB + 回读 8 MB"的固定成本。选后端的经验法则：
> **面板尺寸的区域用 GPU，全窗级的大区域两边一样快**。

各阶段耗时（CPU，1080p，`EFFIBUDDY_BACKDROP_DEBUG=1`）：

```
downsample 0.30ms | blur 0.08ms | composite 1.16ms | total 1.55ms
```

也就是说成本已经几乎全部是**内存带宽**：写 8 MB 输出 + 读 0.5 MB 低分辨率图，
1.16 ms ≈ 7.3 GB/s，接近本机单线程流式写上限（固定长度的 u32 写测试约 11 GB/s）。
这也是为什么再优化算法的边际收益很小，而 GPU 后端在 1080p 上并不比 CPU 快：
它同样被"上传 8 MB + 回读 8 MB"卡住。

性能相关的几个实现决定：

1. **行分块 + rayon**，每行一次 `par_chunks_mut`，按行缓存友好；
   垂直卷积也按行做（每像素读 `2·radius+1` 行，落在 L1/L2 内）。
2. **每帧预计算列采样计划**（`low_x0 / low_x1 / t`），内层循环不再做 floor/clamp。
3. **单次 4 字节写出**，四个通道先打包成 `[u8; 4]` 再 `copy_from_slice`，避免四次边界检查。
4. **单态化内层循环**：`composite_impl::<MASK, IDENTITY>`，四种组合各编译一份，
   内层没有分支，LLVM 可以做通道向量化。
5. GPU 端 **每个 dispatch 一个 compute pass**：blade 在 pass 结束时插内存屏障，
   四个 pass 才能安全地把 buffer 交给下一个。

## 4. GPU 后端与 `vendor/blade-graphics`

GPU 后端用 `blade-graphics`（GPUI 在同机上用的就是它）跑 4 个 compute dispatch：

| pass | 内容 |
| --- | --- |
| `downsample` | n×n 盒式平均（读源区域，写 1/n² 缓冲） |
| `blur_h` / `blur_v` | 可分离定点高斯（浮点权重来自 `Plan`，与 CPU 完全一致） |
| `composite` | 双线性放大 + 调色矩阵 + 色纱 + 圆角 SDF，写回宿主机可见缓冲 |

**为什么要打补丁。** 回读是这条链路上唯一"超出 GPU 控制"的部分。blade 0.7 只提供
`Device / Shared / Upload` 三种显存类别，而 gpu-alloc 的选择策略让这三种在回读场景下
都落在慢速内存上。本机实测（`cargo run --release --example memtypes`，8 MB memset+memcpy）：

| 内存类型 | 回读带宽 |
| --- | --- |
| `DEVICE_LOCAL \| HOST_VISIBLE`（BAR，`Memory::Shared` 会选中它） | 0.04 GB/s |
| `HOST_VISIBLE \| HOST_COHERENT`（`Memory::Upload` 选中它） | 0.29 GB/s |
| `HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED` | **58 GB/s** |

即"映射了但没进 CPU cache"，8 MB 回读要 190 ms。gpu-alloc 只有在 usage 里带
`DOWNLOAD` 标志时才倾向 `HOST_CACHED`，而 blade 没有对应的 `Memory` 变体。
因此 `vendor/blade-graphics` 加了一个变体：

```rust
Memory::Download  =>  gpu_alloc::UsageFlags::HOST_ACCESS | gpu_alloc::UsageFlags::DOWNLOAD
```

补丁是纯粹的"新增分支"：不改变任何现有 API 行为，GPUI 的渲染路径不受影响
（`cargo check -p effibuddy-ui` 通过）。加了之后 1080p 回读从 190 ms 降到 ~0.15 ms。

GPU 后端的其余缓冲：源图 `Shared`（CPU 顺序写入很快，实测 10 GB/s）、低分辨率工作集
`Device`（不出显存）、合成结果 `Device` + 一次 `copy_buffer_to_buffer` 到 `Download`
暂存缓冲（DMA 走 PCIe，比映射大缓冲快）。

## 5. 验证

```bash
cargo test -p effibuddy-backdrop          # 23 个单元测试 + 4 个 CPU/GPU 一致性测试
cargo test -p effibuddy-ui glass          # 设计系统封装
cargo run --release -p effibuddy-backdrop --bin backdrop-bench
```

覆盖点：

- 高斯核归一化（`sum == 1<<16`）、对称性；
- 纯色背景过模糊后仍是纯色（不会因边缘 clamp 变暗）；
- 棋盘格方差显著下降（真的在模糊）；
- 圆角遮罩：角像素 alpha=0、中心 alpha=255；
- 子区域视图与原图裁剪结果一致（±1 LSB）；
- 调色矩阵与"逐项 apply"的参考实现一致（±1e-5）；
- GPU 与 CPU 输出：均值差 ≤ 1.5/255，最大差 ≤ 6/255（CPU 用 16.16 定点截断，GPU 用 f32 四舍五入）。

## 6. 窗口内实时毛玻璃（渲染器集成）

`crates/backdrop` 处理的是"你手上已有的一份 RGBA"，而应用里真正的毛玻璃需要**渲染时刻的帧缓冲**。
GPUI 原本不暴露这个能力，所以 `vendor/gpui` 里加了一个真正的 backdrop 图元，用法和 CSS 一致：

```rust
div()
    .w(px(396.0))
    .rounded(px(14.0))
    .border_1()
    .bg(theme.elevated.opacity(0.72))            // 半透明底，让玻璃透出来
    .backdrop_filter(px(18.0), theme.elevated.opacity(0.22))  // 模糊 + 色纱
    .child(/* 卡片内容:文字保持清晰 */)
```

`crates/app/src/app.rs` 的「赞赏（捐赠）」弹窗已经改用这个写法。

### 6.1 实现方式

GPUI 的场景是按 `(order, kind)` 归并成批次的，`PrimitiveKind::BackdropFilter` 排在 `Quad` 之前，
所以它在"背景已画完、面板自身还没画"的准确时刻落到渲染循环里：

```
main pass (Quads/Sprites... 已经画了面板背后的所有内容)
  │  ← 遇到 BackdropFilters 批次
  ├─ end pass
  ├─ copy_texture_to_texture: 帧缓冲的 [面板矩形] → backdrop_capture（整窗大小）
  ├─ render pass #1: capture → blur_a（1/4 分辨率，水平 9-tap 高斯，scissor 限制在面板内）
  ├─ render pass #2: blur_a → blur_b（垂直 9-tap）
  ├─ re-open main pass，InitOp::Load
  └─ 用 vs_quad + fs_backdrop 画面板：采样 blur_b，叠 tint，按 corner radius 做 SDF 覆盖
        ↓
  之后的内容（文字、边框、子元素）正常画在玻璃之上
```

要点：

- **顺序天然正确**：批次合并按 order 排序，所以"背后"= 之前画的全部内容，"上面"= 之后画的内容，
  和 CSS 的语义一致；`PrimitiveKind::BackdropFilter` 排在同 order 的 `Quad` 之前，面板自身的
  背景/边框不会把自己糊掉。
- **成本与面板面积成正比**：一次矩形拷贝 + 两个 1/4 分辨率的 9-tap pass。640×420 的面板约
  160×105 个工作像素（≈0.3M 次采样），实测远低于 0.1 ms，可以跟随滚动/动画每帧重算。
- **sRGB 一致**：capture/blur 纹理都用 surface 的格式，模糊在线性空间做，与 GPUI 其它管线一致。
- **边缘 clamp**：两次 blur 与最终采样都把 uv 夹在面板矩形内，等价 CSS 在元素边界截断背板的行为。
- **换窗口尺寸**：三张纹理随 drawable resize 一起重建（`update_drawable_size_impl`）。

配套改动（都记在 `vendor/gpui` 里，与本仓库其它 vendored 补丁同一套路）：

| 文件 | 改动 |
| --- | --- |
| `scene.rs` | 新增 `BackdropFilter` 图元、`PrimitiveKind::BackdropFilter`、批次合并 |
| `style.rs` / `styled.rs` | `Style.backdrop_filter` + `Styled::backdrop_filter(Pixels, Hsla)` |
| `window.rs` | `Window::paint_backdrop_filter` |
| `blade_renderer.rs` | 三张 capture/blur 纹理、`backdrop` / `backdrop_blur` 管线、拷贝+两 pass 的绘制臂 |
| `shaders.wgsl` | `vs_fullscreen` / `fs_blur`（9-tap 高斯）/ `fs_backdrop`（采样+色纱+圆角） |

### 6.2 与 `crates/backdrop` 的关系

两者刻意分工：

- **渲染器内联版本**（`vendor/gpui`）：只做"面板矩形 + 固定 9-tap 1/4 分辨率模糊 + 色纱 + 圆角"，
  因为它必须每帧、每个面板都跑，且输入是 GPU 上的帧缓冲，回读没有意义。
- **`crates/backdrop`**：处理 CPU 侧的 RGBA 源（壁纸、图表快照、截图预览），有 CPU/GPU 两个后端、
  自动降采样、自适应帧预算，以及可测试的一致性保证。它也是本文档第 1~5 节那套算法与实测数据的来源，
  渲染器里的 1/4 分辨率 + 可分离高斯正是同一思路的简化版。

### 6.3 手动验证

```bash
cargo run --example backdrop_check -p effibuddy-ui
```

窗口里是一条彩色条纹背景 + 一块玻璃面板：面板内的条纹必须被糊掉、面板外的条纹保持锐利、
面板上的文字保持锐利、圆角处有抗锯齿。

### 6.4 已知限制

- 玻璃面板里的内容**不能**再包含玻璃面板（嵌套 backdrop 会在同一次 capture 里重复糊一次）。
- 面板区域被 `content_mask` 裁剪时，背板只按面板*未裁切*的 bounds 拷贝，超出的部分不会画出来。
- 多个玻璃面板各自拷贝一次矩形；同屏大量小面板（比如几十个菜单项）建议合并成一个面板。
