use std::fs::File;
use std::io::Write;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icon.png");

    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into());
    let icons_rs_path = Path::new(&out_dir).join("icons.rs");

    // 初始颜色：淡蓝色 (#38BDF8 / RGB: 56, 189, 248)
    const BLUE: (u8, u8, u8) = (56, 189, 248);
    // 变灰颜色：中性灰 (#9196A0 / RGB: 145, 150, 160)
    const GRAY: (u8, u8, u8) = (145, 150, 160);

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let icon_png_path = Path::new(&manifest_dir).join("dist").join("logo.png");
    let icon_png_path = if icon_png_path.is_file() {
        icon_png_path
    } else {
        Path::new(&manifest_dir).join("icon.png")
    };

    let alpha_mask = load_icon_alpha_mask(&icon_png_path);

    let (blue_32, gray_32, blue_16, gray_16) = if let Some((ref mask, w, h, bbox)) = alpha_mask {
        (
            render_scaled_icon(mask, w, h, bbox, 32, BLUE),
            render_scaled_icon(mask, w, h, bbox, 32, GRAY),
            render_scaled_icon(mask, w, h, bbox, 16, BLUE),
            render_scaled_icon(mask, w, h, bbox, 16, GRAY),
        )
    } else {
        (
            render_fallback_diamond(32, BLUE),
            render_fallback_diamond(32, GRAY),
            render_fallback_diamond(16, BLUE),
            render_fallback_diamond(16, GRAY),
        )
    };

    let mut icons_rs = File::create(&icons_rs_path).expect("Failed to create icons.rs");
    writeln!(icons_rs, "pub static BLUE_ICON_32: [u8; {}] = {:?};", blue_32.len(), blue_32).unwrap();
    writeln!(icons_rs, "pub static GRAY_ICON_32: [u8; {}] = {:?};", gray_32.len(), gray_32).unwrap();
    writeln!(icons_rs, "pub static BLUE_ICON_16: [u8; {}] = {:?};", blue_16.len(), blue_16).unwrap();
    writeln!(icons_rs, "pub static GRAY_ICON_16: [u8; {}] = {:?};", gray_16.len(), gray_16).unwrap();

    // 生成淡蓝色 app.ico (包含 16, 24, 32, 48, 64 多分辨率，顶格充满)
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
            let ico_path = Path::new(&manifest_dir).join("app.ico");
            generate_ico(&alpha_mask, BLUE, ico_path.to_str().unwrap());

            // 尝试通过 winres 编译 PE 资源 (若环境安装了 rc.exe / windres.exe)
            let mut res = winres::WindowsResource::new();
            res.set_icon(ico_path.to_str().unwrap());
            let _ = res.compile();
        }
    }
}

/// BoundingBox: (min_x, max_x, min_y, max_y)
type BoundingBox = (usize, usize, usize, usize);

/// 从 icon.png 读取 Alpha 掩码并计算实际有效像素的外接矩形
fn load_icon_alpha_mask(path: &Path) -> Option<(Vec<u8>, usize, usize, BoundingBox)> {
    let file = File::open(path).ok()?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;

    let w = info.width as usize;
    let h = info.height as usize;
    let bytes = &buf[..info.buffer_size()];

    let mut alpha_mask = vec![0u8; w * h];
    let mut min_x = w;
    let mut max_x = 0;
    let mut min_y = h;
    let mut max_y = 0;

    for y in 0..h {
        for x in 0..w {
            let a = match info.color_type {
                png::ColorType::Rgba => bytes[(y * w + x) * 4 + 3],
                png::ColorType::GrayscaleAlpha => bytes[(y * w + x) * 2 + 1],
                png::ColorType::Rgb => {
                    let r = bytes[(y * w + x) * 3];
                    let g = bytes[(y * w + x) * 3 + 1];
                    let b = bytes[(y * w + x) * 3 + 2];
                    if r < 240 || g < 240 || b < 240 { 255 } else { 0 }
                }
                png::ColorType::Grayscale => {
                    let g = bytes[y * w + x];
                    if g < 240 { 255 } else { 0 }
                }
                _ => 255,
            };

            alpha_mask[y * w + x] = a;

            if a > 10 {
                if x < min_x { min_x = x; }
                if x > max_x { max_x = x; }
                if y < min_y { min_y = y; }
                if y > max_y { max_y = y; }
            }
        }
    }

    if min_x <= max_x && min_y <= max_y {
        Some((alpha_mask, w, h, (min_x, max_x, min_y, max_y)))
    } else {
        None
    }
}

/// 按照有效外接矩形“放大到顶格”渲染指定尺寸和颜色的 RGBA 像素数据
/// 保持原图像长宽比，将高度完全充满整个画布 (0..size)，并在水平方向精确居中，配合 4x4 超采样抗锯齿
fn render_scaled_icon(
    alpha_mask: &[u8],
    img_w: usize,
    img_h: usize,
    bbox: BoundingBox,
    target_size: u32,
    color: (u8, u8, u8),
) -> Vec<u8> {
    let (min_x, max_x, min_y, max_y) = bbox;
    let box_w = (max_x - min_x + 1) as f32;
    let box_h = (max_y - min_y + 1) as f32;

    // 高度顶格充满：scale 将 box_h 映射到 target_size
    let scale = box_h / (target_size as f32);
    let scaled_w = box_w / scale;
    let x_offset = (target_size as f32 - scaled_w) / 2.0;

    let mut out = Vec::with_capacity((target_size * target_size * 4) as usize);

    for y in 0..target_size {
        for x in 0..target_size {
            // 4x4 超采样 (Supersampling) 计算平滑 Alpha 边缘
            let mut sum_a = 0.0f32;
            for sy_step in 0..4 {
                let v = y as f32 + (sy_step as f32 + 0.5) / 4.0;
                let src_y = min_y as f32 + v * scale;

                for sx_step in 0..4 {
                    let u = x as f32 + (sx_step as f32 + 0.5) / 4.0;
                    let src_x = min_x as f32 + (u - x_offset) * scale;

                    if src_x >= 0.0 && src_x < (img_w as f32) && src_y >= 0.0 && src_y < (img_h as f32) {
                        let ix = src_x as usize;
                        let iy = src_y as usize;
                        let a = alpha_mask[iy * img_w + ix];
                        sum_a += a as f32;
                    }
                }
            }

            let avg_a = (sum_a / 16.0).round().min(255.0).max(0.0) as u8;

            out.push(color.0);
            out.push(color.1);
            out.push(color.2);
            out.push(avg_a);
        }
    }

    out
}

/// 兜底：纯数学 45° 菱形
fn render_fallback_diamond(size: u32, color: (u8, u8, u8)) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.48;

    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - center).abs();
            let dy = (y as f32 - center).abs();
            let dist = (dx + dy) - radius;

            let alpha = if dist <= -0.5 {
                255
            } else if dist >= 0.5 {
                0
            } else {
                ((0.5 - dist) * 255.0).round() as u8
            };

            out.push(color.0);
            out.push(color.1);
            out.push(color.2);
            out.push(alpha);
        }
    }
    out
}

#[cfg(windows)]
fn generate_ico(
    alpha_mask_opt: &Option<(Vec<u8>, usize, usize, BoundingBox)>,
    color: (u8, u8, u8),
    out_path: &str,
) {
    let sizes = [16u32, 24, 32, 48, 64];
    let mut file = File::create(out_path).expect("Failed to create ico file");

    // ICO 头部 (6 字节)
    file.write_all(&[0, 0, 1, 0, sizes.len() as u8, 0]).unwrap();

    let mut dib_list = Vec::new();

    for &size in &sizes {
        let mut dib = Vec::new();
        let bi_height = (size * 2) as i32;
        let img_size = size * size * 4;

        // BITMAPINFOHEADER (40 字节)
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&(size as i32).to_le_bytes());
        dib.extend_from_slice(&bi_height.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&img_size.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());

        // 渲染 RGBA 像素数据并转换为 Windows DIB 倒序 (bottom-up) BGRA
        let rgba = if let Some((ref mask, w, h, bbox)) = alpha_mask_opt {
            render_scaled_icon(mask, *w, *h, *bbox, size, color)
        } else {
            render_fallback_diamond(size, color)
        };

        for y in (0..size).rev() {
            for x in 0..size {
                let idx = ((y * size + x) * 4) as usize;
                let r = rgba[idx];
                let g = rgba[idx + 1];
                let b = rgba[idx + 2];
                let a = rgba[idx + 3];
                // BGRA
                dib.extend_from_slice(&[b, g, r, a]);
            }
        }

        // AND 掩码 (全 0 表示由 32-bit Alpha 控制透明)
        let mask_row_bytes = ((size + 31) / 32) * 4;
        dib.resize(dib.len() + (mask_row_bytes * size) as usize, 0);

        dib_list.push((size, dib));
    }

    // 写入各尺寸目录项 (每项 16 字节)
    let header_size = 6 + (sizes.len() * 16) as u32;
    let mut current_offset = header_size;

    for (size, dib) in &dib_list {
        let s = if *size >= 256 { 0u8 } else { *size as u8 };
        file.write_all(&[s, s, 0, 0]).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&32u16.to_le_bytes()).unwrap();
        file.write_all(&(dib.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&current_offset.to_le_bytes()).unwrap();
        current_offset += dib.len() as u32;
    }

    // 写入 DIB 数据主体
    for (_, dib) in dib_list {
        file.write_all(&dib).unwrap();
    }
}
