//! 临时诊断（用完即删）：找**回声/多路径**（同一份音频经两条不同延迟的路径到达输出）。
//!
//! 回声在听感上就是「卡/不流畅」，但它对逐路测量是隐形的：每一路自己都干净，
//! 而 1kHz 单音也测不出固定延迟的回声（B = A + g·A(Δ) 仍然是个纯音）。
//! 宽带噪声 + 宽范围互相关就能看出来：主峰在直达延迟处，回声会在 Δ 处再起一个峰。
//!
//! 做法：往虚拟声卡播白噪声，同时采 A（Windows→我们）与 B（Minifuse 输出），
//! 抽取后在 0–3 秒范围内求归一化互相关，列出所有 >0.3 的峰。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiomix_backend_windows::wasapi::capture;
use audiomix_backend_windows::wasapi::device::enumerate_devices;
use audiomix_backend_windows::wasapi::render;
use audiomix_core::backend::StartedStream;
use audiomix_core::model::DeviceKind;

const SECS: f32 = 14.0;
const DEC: usize = 32;

struct XorShift(u64);
impl XorShift {
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // [-1, 1)
        ((x >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
    }
}

fn start(label: &str, id: &str) -> Option<(Arc<Mutex<Vec<f32>>>, f32, StartedStream)> {
    let rec = Arc::new(Mutex::new(Vec::<f32>::new()));
    let r2 = rec.clone();
    let s = capture::start_capture(
        id,
        true,
        Box::new(move |d| {
            let mut v = r2.lock().unwrap();
            for &x in d.iter().step_by(2) {
                v.push(x);
            }
        }),
    )
    .ok()?;
    println!("{label}: {}Hz/{}ch", s.info.sample_rate, s.info.channels);
    Some((rec, s.info.sample_rate as f32, s))
}

fn decimate(x: &[f32], dec: usize) -> Vec<f32> {
    x.chunks(dec).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect()
}

fn main() {
    let devices = match enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举设备失败: {e}");
            return;
        }
    };
    let vc_out = devices
        .iter()
        .find(|d| d.name.contains("Virtual Cable") && d.kind == DeviceKind::Output)
        .cloned();
    let main_out = devices
        .iter()
        .find(|d| d.kind == DeviceKind::Output && d.name.contains("Minifuse"))
        .cloned();
    let (Some(vc_out), Some(main_out)) = (vc_out, main_out) else {
        println!("(缺少线缆播放端或主播放设备)");
        return;
    };

    let a = start("A 虚拟声卡播放端", &vc_out.id);
    let b = start("B Minifuse 输出", &main_out.id);

    // 白噪声（宽带）：只有宽带信号才能在互相关里分辨出多个延迟
    let rng = Arc::new(Mutex::new(XorShift(0x2545_f491_4f6c_dd1d)));
    let ch = vc_out.channels.max(1) as usize;
    let play = render::start_render(
        &vc_out.id,
        Box::new(move |out| {
            let mut r = rng.lock().unwrap();
            for frame in out.chunks_mut(ch) {
                let v = r.next_f32() * 0.2;
                for y in frame.iter_mut() {
                    *y = v;
                }
            }
        }),
    );
    let _play = match play {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开线缆播放端失败: {e}");
            return;
        }
    };
    println!("\n播白噪声 {SECS}s，同时采两路…");
    std::thread::sleep(Duration::from_secs_f32(SECS));
    drop(_play);

    let (Some((da, fa, _ha)), Some((db, fb, _hb))) = (&a, &b) else {
        println!("某一路没起来");
        return;
    };
    if (*fa - *fb).abs() > 1.0 {
        println!("两路采样率不同（{fa} vs {fb}），跳过");
        return;
    }
    let fs = *fa;
    let a = decimate(&da.lock().unwrap(), DEC);
    let b = decimate(&db.lock().unwrap(), DEC);
    let fs_d = fs / DEC as f32;
    let n = a.len().min(b.len());
    println!("抽取后：{} 点 @ {fs_d:.0}Hz（{:.1}s）", n, n as f32 / fs_d);

    let max_lag = (fs_d * 3.0) as usize; // 最多找 3 秒
    // 用**短窗口**（1 秒）分别求互相关再平均：两路是不同设备的采集流，
    // 时钟漂移会让长窗口的相关系数崩掉（14 秒窗口实测只有 0.2）。
    let wlen = fs_d as usize; // 1 秒
    let starts = [
        (fs_d * 2.0) as usize,
        (fs_d * 5.0) as usize,
        (fs_d * 8.0) as usize,
        (fs_d * 11.0) as usize,
    ];
    let mut curve = vec![0f32; max_lag.min(n / 2)];
    let mut used = 0;
    for &s in &starts {
        if s + wlen + curve.len() >= n {
            continue;
        }
        used += 1;
        let aw = &a[s..s + wlen];
        for (lag, slot) in curve.iter_mut().enumerate() {
            let bw = &b[s + lag..s + lag + wlen];
            let mut dot = 0f64;
            let mut na = 0f64;
            let mut nb = 0f64;
            for i in 0..wlen {
                let x = aw[i] as f64;
                let y = bw[i] as f64;
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            *slot += if na > 0.0 && nb > 0.0 { (dot / (na.sqrt() * nb.sqrt())) as f32 } else { 0.0 };
        }
    }
    if used > 0 {
        for v in curve.iter_mut() {
            *v /= used as f32;
        }
    }
    println!("（{used} 个 1 秒窗口平均）");

    // 找峰：先是全局最大，再列出其它 >0.3 且是局部极大的延迟
    let mut order: Vec<usize> = (0..curve.len()).collect();
    order.sort_by(|x, y| curve[*y].partial_cmp(&curve[*x]).unwrap());
    println!("\n最强的几个互相关峰（延迟 / 相关系数）：");
    let mut shown = 0;
    let mut taken: Vec<usize> = Vec::new();
    for &i in &order {
        if shown >= 6 {
            break;
        }
        // 与已列出的峰至少要相隔 20ms 才算另一个峰
        if taken.iter().any(|t| (*t as i64 - i as i64).abs() < (fs_d * 0.02) as i64) {
            continue;
        }
        taken.push(i);
        println!(
            "  {:8.1} ms   相关系数 {:.3}{}",
            i as f32 / fs_d * 1000.0,
            curve[i],
            if shown == 0 { "   ← 直达路径" } else { "" }
        );
        shown += 1;
    }
    // 峰之间的间隔（回声就是"额外的一个峰"）
    if taken.len() >= 2 {
        let d0 = taken[0] as f32 / fs_d * 1000.0;
        for &t in &taken[1..] {
            let d = t as f32 / fs_d * 1000.0;
            println!("  峰间距：{:.1} ms（相对直达）", d - d0);
        }
    }
    // 分段最大值：直达路径在 0–1s 内，回声会出现在更远的区间
    for (name, lo, hi) in [("0–0.6s", 0.0, 0.6), ("0.6–1.2s", 0.6, 1.2), ("1.2–2s", 1.2, 2.0), ("2–3s", 2.0, 3.0)] {
        let l = (fs_d * lo) as usize;
        let h = ((fs_d * hi) as usize).min(curve.len());
        if l >= h {
            continue;
        }
        let (mut bi, mut bv) = (l, f32::MIN);
        for i in l..h {
            if curve[i] > bv {
                bv = curve[i];
                bi = i;
            }
        }
        println!("  {name:>8} 区间最大：{:.1} ms → {:.3}", bi as f32 / fs_d * 1000.0, bv);
    }
    println!(
        "\n结论：{}",
        if shown >= 2 && curve[taken[1]] > 0.3 {
            "⚠️ 存在第二个明显峰 → 很可能有回声/多路径（听感=卡）"
        } else {
            "✅ 只有一个相关峰 → 没有可测量的回声/多路径"
        }
    );
}
