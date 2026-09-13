//! 临时诊断（用完即删）：把两个 WAV（A=Windows→我们、M=我们的麦克风端输出）对齐后逐样本比较。
//!
//! 两个端点其实是**同一个虚拟设备**（时钟都来自我们的节拍），所以不存在设备间漂移，
//! 对齐后应当高度一致；不一致的地方就是"卡"的病灶。

use std::fs;

fn read_wav(path: &str) -> Option<(Vec<f32>, u32, usize, Vec<f32>)> {
    let raw = fs::read(path).ok()?;
    let mut pos = 12usize;
    let mut data_start = 44usize;
    let mut ch = 2usize;
    let mut rate = 48000u32;
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let sz = u32::from_le_bytes([raw[pos + 4], raw[pos + 5], raw[pos + 6], raw[pos + 7]]) as usize;
        if id == b"fmt " {
            ch = u16::from_le_bytes([raw[pos + 10], raw[pos + 11]]) as usize;
            rate = u32::from_le_bytes([raw[pos + 12], raw[pos + 13], raw[pos + 14], raw[pos + 15]]);
        } else if id == b"data" {
            data_start = pos + 8;
            break;
        }
        pos += 8 + sz + (sz & 1);
    }
    let n = (raw.len() - data_start) / 2;
    let frames = n / ch;
    let mut l = Vec::with_capacity(frames);
    let mut r = Vec::with_capacity(frames);
    for i in 0..frames {
        let off = data_start + (i * ch) * 2;
        l.push(i16::from_le_bytes([raw[off], raw[off + 1]]) as f32 / 32768.0);
        let off2 = data_start + (i * ch + (ch - 1)) * 2;
        r.push(i16::from_le_bytes([raw[off2], raw[off2 + 1]]) as f32 / 32768.0);
    }
    Some((l, rate, ch, r))
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn main() {
    let a_path = std::env::args().nth(1).unwrap_or_else(|| r"H:\Temp\AudioMix\A_虚拟声卡播放端.wav".into());
    let m_path = std::env::args().nth(2).unwrap_or_else(|| r"H:\Temp\AudioMix\M_虚拟麦克风端.wav".into());
    let (Some((a, ra, _, ar)), Some((m, rm, _, mr))) = (read_wav(&a_path), read_wav(&m_path)) else {
        println!("读文件失败");
        return;
    };
    println!("A: {} 样本 ({:.2}s) {a_path}", a.len(), a.len() as f32 / ra as f32);
    println!("M: {} 样本 ({:.2}s) {m_path}", m.len(), m.len() as f32 / rm as f32);
    if ra != rm {
        println!("采样率不同，跳过");
        return;
    }
    let rate = ra as f32;
    let n = a.len().min(m.len());

    // 粗对齐（抽取 16）
    let dec = 16usize;
    let ad: Vec<f32> = a[..n].chunks(dec).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    let md: Vec<f32> = m[..n].chunks(dec).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    let max_lag = ((rate as f32 / dec as f32) * 8.0) as usize; // 最多 8 秒（麦克风端可能严重滞后）
    let mut best = (0usize, f32::MIN);
    for lag in 0..max_lag.min(ad.len() / 2) {
        let len = ad.len() - lag;
        let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
        for i in 0..len {
            dot += ad[i] * md[i + lag];
            na += ad[i] * ad[i];
            nb += md[i + lag] * md[i + lag];
        }
        let c = if na > 0.0 && nb > 0.0 { dot / (na.sqrt() * nb.sqrt()) } else { 0.0 };
        if c > best.1 {
            best = (lag, c);
        }
    }
    let coarse = best.0 * dec;
    // 细化
    let mut lag_best = coarse;
    let mut corr_best = f32::MIN;
    let lo = coarse.saturating_sub(dec * 2);
    let hi = (coarse + dec * 2).min(n - 1000);
    for lag in lo..=hi {
        let len = n - lag;
        let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
        for i in 0..len {
            dot += a[i] as f64 * m[i + lag] as f64;
            na += a[i] as f64 * a[i] as f64;
            nb += m[i + lag] as f64 * m[i + lag] as f64;
        }
        let c = if na > 0.0 && nb > 0.0 { (dot / (na.sqrt() * nb.sqrt())) as f32 } else { 0.0 };
        if c > corr_best {
            corr_best = c;
            lag_best = lag;
        }
    }
    println!(
        "\n对齐：M 比 A 晚 {:.1} ms（{} 样本），整体相关 {:.4}",
        lag_best as f32 / rate * 1000.0,
        lag_best,
        corr_best
    );

    // 四向相关：判断麦克风端是否交换/混合了左右声道
    let corr_at = |x: &[f32], y: &[f32], lag: usize| -> f32 {
        let len = x.len().min(y.len().saturating_sub(lag));
        let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
        for i in 0..len {
            dot += x[i] as f64 * y[i + lag] as f64;
            na += x[i] as f64 * x[i] as f64;
            nb += y[i + lag] as f64 * y[i + lag] as f64;
        }
        if na > 0.0 && nb > 0.0 {
            (dot / (na.sqrt() * nb.sqrt())) as f32
        } else {
            0.0
        }
    };
    let lag = lag_best;
    let ll = corr_at(&a, &m, lag);
    let lr = corr_at(&a, &mr, lag);
    let rl = corr_at(&ar, &m, lag);
    let rr = corr_at(&ar, &mr, lag);
    println!(
        "四向相关（A左/A右 × M左/M右）：左-左 {ll:.3}  左-右 {lr:.3}  右-左 {rl:.3}  右-右 {rr:.3}"
    );
    println!(
        "  → {}",
        if lr.max(rl) > ll.max(rr) + 0.2 {
            "⚠️ 左右声道被交换了！"
        } else if ll.min(rr) < 0.5 && (ll + rr) / 2.0 > lr.max(rl) {
            "⚠️ M 的左右都能与 A 对上，但相关偏低"
        } else {
            "声道没有交换"
        }
    );

    // 先看两边的包络（100ms RMS），直观判断内容/延迟是否对得上
    let w100 = (rate * 0.1) as usize;
    println!("\n包络对照（100ms RMS）：t   A        M");
    let mut i = 0usize;
    let mut shown = 0;
    while i + w100 <= n.min(m.len()) && shown < 70 {
        let ra_ = rms(&a[i..i + w100]);
        let rm_ = rms(&m[i..i + w100]);
        println!("  {:5.1}s  {:.4}   {:.4}", i as f32 / rate, ra_, rm_);
        i += w100;
        shown += 1;
    }

    // 逐窗口（0.25s）各自微调延迟，然后算相关与残差
    let win = (rate * 0.25) as usize;
    let search = 64i32;
    let mut lag = lag_best as i32;
    let mut rows: Vec<(f32, f32, f32, f32, f32)> = Vec::new(); // (t, corr, resid_rel, gain, lag_ms)
    let mut start = 0usize;
    while start + win < n && (start + win + lag as usize) < m.len() {
        let aw = &a[start..start + win];
        if rms(aw) < 0.002 {
            start += win;
            continue;
        }
        let mut bb = (lag, f32::MIN, 1.0f32);
        for l in (lag - search)..=(lag + search) {
            let lo2 = start as i32 + l;
            if lo2 < 0 || lo2 as usize + win >= m.len() {
                continue;
            }
            let mw = &m[lo2 as usize..lo2 as usize + win];
            let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
            for i in 0..win {
                dot += aw[i] as f64 * mw[i] as f64;
                na += aw[i] as f64 * aw[i] as f64;
                nb += mw[i] as f64 * mw[i] as f64;
            }
            let c = if na > 0.0 && nb > 0.0 { (dot / (na.sqrt() * nb.sqrt())) as f32 } else { 0.0 };
            if c > bb.1 {
                bb = (l, c, if na > 0.0 { (dot / na) as f32 } else { 1.0 });
            }
        }
        lag = bb.0;
        let mw = &m[(start as i32 + lag) as usize..(start as i32 + lag) as usize + win];
        let g = bb.2;
        let resid: Vec<f32> = (0..win).map(|i| mw[i] - g * aw[i]).collect();
        let rel = rms(&resid) / rms(mw).max(1e-9);
        rows.push((start as f32 / rate, bb.1, rel, g, lag as f32 / rate * 1000.0));
        start += win;
    }
    if rows.is_empty() {
        println!("没有有效窗口");
        return;
    }
    // 关键指标：每个窗口的对齐延迟是否稳定（抖动 = time-warp = 听感"卡/不流畅"）
    println!("\n逐窗口对齐延迟（ms）：");
    let lags_ms: Vec<f32> = rows.iter().map(|r| r.4).collect();
    for (i, r) in rows.iter().enumerate() {
        if i % 2 == 0 {
            print!("{:6.2}s:{:7.1}  ", r.0, r.4);
            if i % 8 == 6 {
                println!();
            }
        }
    }
    println!();
    let mut ls = lags_ms.clone();
    ls.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  延迟：最小 {:.1}ms / 中位 {:.1}ms / 最大 {:.1}ms；**峰峰值抖动 {:.1}ms**",
        ls[0],
        ls[ls.len() / 2],
        ls[ls.len() - 1],
        ls[ls.len() - 1] - ls[0]
    );
    let mut cors: Vec<f32> = rows.iter().map(|r| r.1).collect();
    cors.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut res: Vec<f32> = rows.iter().map(|r| r.2).collect();
    res.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "逐 0.25s 窗口（{} 个）：相关 最小 {:.3} / 中位 {:.3}；残差占比 中位 {:.3} / 最差 {:.3}",
        rows.len(),
        cors[0],
        cors[cors.len() / 2],
        res[res.len() / 2],
        res[res.len() - 1]
    );
    println!("\n最差的 12 个窗口：");
    let mut sorted = rows.clone();
    sorted.sort_by(|x, y| x.1.partial_cmp(&y.1).unwrap());
    for (t, c, r, g, _) in sorted.iter().take(12) {
        println!("  t={:6.2}s  相关 {:.3}  残差 {:.3}  增益 {:.3}", t, c, r, g);
    }
    let bad = rows.iter().filter(|r| r.1 < 0.9).count();
    println!(
        "\n结论：{}（相关<0.9 的窗口 {}/{}）",
        if bad == 0 {
            "✅ M 与 A 对齐后高度一致 → 麦克风端没有改信号"
        } else if bad * 4 < rows.len() {
            "⚠️ 少数窗口对不上（看上面的时刻）"
        } else {
            "❌ 大量窗口对不上 → 麦克风端确实在损坏信号"
        },
        bad,
        rows.len()
    );
}
