"""生成 DiskDoctor 的应用图标。

图标形态：仪表盘式的磁盘占用环 —— 一圈表示"容量"，其中一段高亮表示
"已占用的部分"，中心一根指针指向它。选这个形态是因为它在 16×16 下
仍然可辨认（只有一个环 + 一根指针），而且直接表达产品在做的事：
看清磁盘被什么占满了。

用法：
    python assets/make_icon.py [输出路径]

需要 Pillow。产物是多尺寸 ICO（Windows 快捷方式与程序图标都能用）。
"""

from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image, ImageDraw

# 尺寸按 Windows 实际会用到的大小排：任务栏 16/24/32、资源管理器 48/64、
# 大图标视图 128/256。全部塞进一个 ICO，系统按需取用。
SIZES = [256, 128, 64, 48, 32, 24, 16]

# 配色：与 survey 界面一致（深底 + 青蓝主色 + 琥珀警示色）
BG = (26, 31, 43, 255)
RING = (108, 122, 145, 255)
RING_ACTIVE = (88, 196, 221, 255)
NEEDLE = (240, 176, 64, 255)
DOT = (240, 244, 250, 255)


def render(size: int) -> Image.Image:
    """按给定边长绘制一张图。用 4 倍超采样再缩小，边缘才不会有锯齿。"""
    ss = 4
    s = size * ss
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # ---- 圆角方底 ----
    radius = int(s * 0.22)
    d.rounded_rectangle([0, 0, s - 1, s - 1], radius=radius, fill=BG)

    cx = cy = s / 2
    r_out = s * 0.335
    width = s * 0.105
    r_in = r_out - width

    # ---- 占用环：整圈浅灰，其中约 1/3 用青色表示"已占用" ----
    box_out = [cx - r_out, cy - r_out, cx + r_out, cy + r_out]
    d.ellipse(box_out, fill=RING)
    d.pieslice(box_out, -90, 30, fill=RING_ACTIVE)
    # 挖出内圈，让实心圆变成圆环
    box_in = [cx - r_in, cy - r_in, cx + r_in, cy + r_in]
    d.ellipse(box_in, fill=BG)

    # ---- 指针：从圆心指向高亮段的起始（-90°，即正上方）----
    tip = cy - r_in * 0.86
    shaft = s * 0.028
    d.line([cx, cy, cx, tip], fill=NEEDLE, width=int(shaft))
    # 圆头收尾（line 的端点默认是方的）
    d.ellipse(
        [cx - shaft / 2, tip - shaft / 2, cx + shaft / 2, tip + shaft / 2],
        fill=NEEDLE,
    )

    # ---- 中心轴点 ----
    dot = s * 0.055
    d.ellipse([cx - dot, cy - dot, cx + dot, cy + dot], fill=DOT)

    return img.resize((size, size), Image.LANCZOS)


def build(out_path: Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    frames = [render(s) for s in SIZES]
    # 以最大尺寸为基础保存，其余作为附带尺寸塞进去
    frames[0].save(
        out_path,
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=frames[1:],
    )
    print(f"已生成: {out_path}  ({out_path.stat().st_size / 1024:.1f} KB, {len(SIZES)} 种尺寸)")


if __name__ == "__main__":
    target = (
        Path(sys.argv[1])
        if len(sys.argv) > 1
        else Path(__file__).resolve().parent / "diskdoctor.ico"
    )
    build(target)
