//! 九宫格缩放：四角保持原像素，边与中心按最近邻拉伸。
//!
//! 不做滤波，保证皮肤作者设计的角部装饰不会被插值破坏。

use tiny_skia::Pixmap;

/// 边距必须小于背景尺寸，且中间留出可拉伸区域。
pub fn validate_insets(src_width: u32, src_height: u32, insets: [u32; 4]) -> Result<(), String> {
    let [left, top, right, bottom] = insets;
    if left.saturating_add(right) >= src_width || top.saturating_add(bottom) >= src_height {
        return Err("nine_patch insets exceed background size".into());
    }
    Ok(())
}

pub fn scale_nine_patch(
    source: &Pixmap,
    insets: [u32; 4],
    target_width: u32,
    target_height: u32,
) -> Result<Pixmap, String> {
    validate_insets(source.width(), source.height(), insets)?;
    let [left, top, right, bottom] = insets;
    if target_width < left.saturating_add(right) || target_height < top.saturating_add(bottom) {
        return Err("target size is smaller than nine-patch corners".into());
    }
    let mut output = Pixmap::new(target_width, target_height)
        .ok_or_else(|| "target size must not be zero".to_string())?;
    let src_width = source.width();
    let src_height = source.height();
    let (src, dst) = (source.data(), output.data_mut());
    for y in 0..target_height {
        let sy = map_coord(y, target_height, src_height, top, bottom);
        for x in 0..target_width {
            let sx = map_coord(x, target_width, src_width, left, right);
            let from = ((sy as usize * src_width as usize) + sx as usize) * 4;
            let to = ((y as usize * target_width as usize) + x as usize) * 4;
            dst[to..to + 4].copy_from_slice(&src[from..from + 4]);
        }
    }
    Ok(output)
}

pub fn resize_nearest(source: &Pixmap, target_width: u32, target_height: u32) -> Pixmap {
    let out_width = target_width.max(1);
    let out_height = target_height.max(1);
    let mut output = Pixmap::new(out_width, out_height).expect("non-zero target size");
    let src_width = source.width();
    let src_height = source.height();
    let (src, dst) = (source.data(), output.data_mut());
    for y in 0..out_height {
        let sy = (u64::from(y) * u64::from(src_height) / u64::from(out_height)) as u32;
        for x in 0..out_width {
            let sx = (u64::from(x) * u64::from(src_width) / u64::from(out_width)) as u32;
            let from = ((sy as usize * src_width as usize) + sx as usize) * 4;
            let to = ((y as usize * out_width as usize) + x as usize) * 4;
            dst[to..to + 4].copy_from_slice(&src[from..from + 4]);
        }
    }
    output
}

fn map_coord(dst: u32, dst_len: u32, src_len: u32, before: u32, after: u32) -> u32 {
    if dst < before {
        return dst.min(src_len.saturating_sub(1));
    }
    if dst >= dst_len - after {
        return src_len
            .saturating_sub(dst_len - dst)
            .min(src_len.saturating_sub(1));
    }
    let inner_dst = (dst_len - before - after).max(1);
    let inner_src = src_len.saturating_sub(before + after).max(1);
    let offset = ((u64::from(dst - before) * u64::from(inner_src)) / u64::from(inner_dst)) as u32;
    (before + offset).min(src_len.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn striped(width: u32, height: u32) -> Pixmap {
        let mut pixmap = Pixmap::new(width, height).unwrap();
        for y in 0..height {
            for x in 0..width {
                let offset = ((y as usize * width as usize) + x as usize) * 4;
                pixmap.data_mut()[offset] = (x * 10) as u8;
                pixmap.data_mut()[offset + 1] = (y * 10) as u8;
                pixmap.data_mut()[offset + 2] = 77;
                pixmap.data_mut()[offset + 3] = 255;
            }
        }
        pixmap
    }

    #[test]
    fn corners_are_copied_verbatim_and_center_stretches() {
        let source = striped(4, 4);
        let result = scale_nine_patch(&source, [1, 1, 1, 1], 8, 8).unwrap();
        for (sx, sy, dx, dy) in [(0, 0, 0, 0), (3, 0, 7, 0), (0, 3, 0, 7), (3, 3, 7, 7)] {
            let from = ((sy * 4 + sx) * 4) as usize;
            let to = ((dy * 8 + dx) * 4) as usize;
            assert_eq!(
                &source.data()[from..from + 4],
                &result.data()[to..to + 4],
                "corner ({sx},{sy}) moved"
            );
        }
        let left = ((4 * 8) * 4) as usize;
        let right = ((4 * 8 + 7) * 4) as usize;
        assert_ne!(
            &result.data()[left..left + 4],
            &result.data()[right..right + 4]
        );
    }

    #[test]
    fn rejects_insets_larger_than_source_and_tiny_targets() {
        let source = striped(4, 4);
        assert!(validate_insets(4, 4, [3, 1, 1, 1]).is_err());
        assert!(scale_nine_patch(&source, [2, 2, 2, 2], 3, 8).is_err());
    }
}
