//! 纯混音算法（可单元测试）：累加、增益、软限幅、通道变换。

/// 把 input 乘以 gain 后累加到 out（长度以较短者为准）。
pub fn mix_into(out: &mut [f32], input: &[f32], gain: f32) {
    for (o, &i) in out.iter_mut().zip(input.iter()) {
        *o += i * gain;
    }
}

/// 软限幅：|x| <= KNEE 直通，超过后用 tanh 平滑压缩到 [-1, 1]。
/// 曲线在 KNEE 处 C1 连续，无爆音。
pub fn soft_clip(buf: &mut [f32]) {
    const KNEE: f32 = 0.95;
    for s in buf.iter_mut() {
        let x = *s;
        if x > KNEE {
            *s = KNEE + (1.0 - KNEE) * ((x - KNEE) / (1.0 - KNEE)).tanh();
        } else if x < -KNEE {
            *s = -(KNEE + (1.0 - KNEE) * ((-x - KNEE) / (1.0 - KNEE)).tanh());
        }
    }
}

/// 帧级峰值电平（0..=1+），用于电平表。
pub fn peak_of(buf: &[f32]) -> f32 {
    buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// 通道数变换：src 通道 interleaved → dst 通道 interleaved。
/// 策略：dst 通道 i 取 src 通道 min(i, src_ch-1)（单声道复制、多声道取前 N）。
pub fn convert_channels(src: &[f32], src_ch: usize, dst: &mut [f32], dst_ch: usize, frames: usize) {
    if src_ch == dst_ch {
        let n = (frames * dst_ch).min(dst.len()).min(src.len());
        dst[..n].copy_from_slice(&src[..n]);
        return;
    }
    for f in 0..frames {
        for c in 0..dst_ch {
            let sc = c.min(src_ch - 1);
            let v = src.get(f * src_ch + sc).copied().unwrap_or(0.0);
            dst[f * dst_ch + c] = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mix_into_gain() {
        let mut out = vec![0.5, 0.5];
        mix_into(&mut out, &[1.0, -1.0], 0.5);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!(out[1].abs() < 1e-6);
    }

    #[test]
    fn test_soft_clip_bounds() {
        let mut buf = vec![0.5, 1.5, -2.0, 0.0];
        soft_clip(&mut buf);
        assert!(buf.iter().all(|&s| s >= -1.0 && s <= 1.0));
        assert!((buf[0] - 0.5).abs() < 1e-6, "小信号不应被改变");
    }

    #[test]
    fn test_soft_clip_continuity() {
        // 0.95 与略大于 0.95 的值应当接近（C1 连续）
        let mut a = vec![0.95];
        let mut b = vec![0.951];
        soft_clip(&mut a);
        soft_clip(&mut b);
        assert!((a[0] - b[0]).abs() < 0.01);
    }

    #[test]
    fn test_convert_channels_mono_to_stereo() {
        let src = [0.1, 0.2, 0.3];
        let mut dst = [0.0f32; 6];
        convert_channels(&src, 1, &mut dst, 2, 3);
        assert_eq!(dst, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn test_convert_channels_stereo_to_mono() {
        let src = [0.1, 0.3, 0.5, 0.7];
        let mut dst = [0.0f32; 2];
        convert_channels(&src, 2, &mut dst, 1, 2);
        assert_eq!(dst, [0.1, 0.5]);
    }

    #[test]
    fn test_peak() {
        assert!((peak_of(&[0.1, -0.9, 0.3]) - 0.9).abs() < 1e-6);
    }
}
