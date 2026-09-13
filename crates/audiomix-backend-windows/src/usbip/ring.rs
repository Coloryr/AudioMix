//! 线缆音频环形缓冲（f32 样本，线程安全）。
//!
//! 语义与 Virtual-Cables internal/audio/ring.go 一致：
//! - 写入侧（ISO OUT 采集 / 混音渲染）永不阻塞，缓冲满时丢弃最旧数据；
//! - 读取侧（ISO IN 应答 / 混音采集线程）不足时补数字静音——
//!   对同步传输而言静音填充优于阻塞 URB。

use parking_lot::Mutex;

struct State {
    buf: Vec<f32>,
    read: usize,
    write: usize,
    size: usize,
    dropped: u64,
    underruns: u64,
    /// 被 trim 主动裁掉的样本数（如果不统计，麦克风端"持续丢样"会完全隐形）
    trimmed: u64,
}

pub struct AudioRing {
    state: Mutex<State>,
}

impl AudioRing {
    /// `capacity_samples` 为 f32 样本容量（帧数 × 通道数）
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            state: Mutex::new(State {
                buf: vec![0.0; capacity_samples.max(1)],
                read: 0,
                write: 0,
                size: 0,
                dropped: 0,
                underruns: 0,
                trimmed: 0,
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.state.lock().buf.len()
    }

    /// 当前可用样本数
    pub fn available(&self) -> usize {
        self.state.lock().size
    }

    pub fn stats(&self) -> (usize, u64, u64) {
        let s = self.state.lock();
        (s.size, s.dropped, s.underruns)
    }

    /// 推入样本；缓冲满则丢弃最旧数据（保最新音频，流不中断）
    pub fn push(&self, data: &[f32]) {
        let mut s = self.state.lock();
        for &v in data {
            if s.size == s.buf.len() {
                s.read = (s.read + 1) % s.buf.len();
                s.size -= 1;
                s.dropped += 1;
            }
            let idx = s.write;
            let next = (idx + 1) % s.buf.len();
            s.buf[idx] = v;
            s.write = next;
            s.size += 1;
        }
    }

    /// 取出最多 out.len() 个样本；不足部分补零（数字静音），
    /// 返回实际读取数。out 清零后填充。
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let mut s = self.state.lock();
        let mut got = 0;
        for slot in out.iter_mut() {
            if s.size == 0 {
                *slot = 0.0;
                continue;
            }
            let next = (s.read + 1) % s.buf.len();
            *slot = s.buf[s.read];
            s.read = next;
            s.size -= 1;
            got += 1;
        }
        if got < out.len() {
            s.underruns += (out.len() - got) as u64;
        }
        got
    }

    pub fn reset(&self) {
        let mut s = self.state.lock();
        s.read = 0;
        s.write = 0;
        s.size = 0;
    }

    /// 只保留最新的 `target` 个样本：把多余的**最旧**样本丢掉。
    ///
    /// 为什么需要它（真机实测的严重缺陷）：录音端环形缓冲在**没有应用读它**的时候
    /// 照样被播放端的 loopback 拷贝灌满；一旦处于"满"状态，之后每次 `push` 都会
    /// 丢掉还没被读走的音频 —— 麦克风端因此丢掉接近 **100%** 的音频，
    /// 听感是持续卡顿（而播放端、USB 到达率、电平全都正常，极难定位）。
    /// 录音端只需要"现在"的音频，所以主动把积压裁掉即可。
    pub fn trim(&self, target: usize) {
        let mut s = self.state.lock();
        while s.size > target {
            s.read = (s.read + 1) % s.buf.len();
            s.size -= 1;
            s.trimmed += 1;
        }
    }

    /// 被 trim 主动裁掉的样本总数（**持续增长 = 录音端在被持续丢样 = 听感卡顿**）
    pub fn trimmed(&self) -> u64 {
        self.state.lock().trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = AudioRing::new(8);
        r.push(&[1.0, 2.0, 3.0]);
        let mut out = [0f32; 4];
        let got = r.pop(&mut out);
        assert_eq!(got, 3);
        assert_eq!(&out[..3], &[1.0, 2.0, 3.0]);
        assert_eq!(out[3], 0.0, "不足部分补静音");
    }

    #[test]
    fn overflow_drops_oldest() {
        let r = AudioRing::new(2);
        r.push(&[1.0, 2.0, 3.0, 4.0]);
        let (avail, dropped, _) = r.stats();
        assert_eq!(avail, 2);
        assert_eq!(dropped, 2);
        let mut out = [0f32; 2];
        r.pop(&mut out);
        assert_eq!(out, [3.0, 4.0]);
    }

    #[test]
    fn empty_is_silence() {
        let r = AudioRing::new(4);
        let mut out = [1f32; 3];
        let got = r.pop(&mut out);
        assert_eq!(got, 0);
        assert_eq!(out, [0.0; 3]);
    }

    /// 录音端的核心修复：积压被裁掉、保留的必须是**最新**的样本，
    /// 否则"环满 → 每次 push 丢掉还没被读走的音频"会让麦克风端持续卡顿。
    #[test]
    fn trim_keeps_newest_and_bounds_occupancy() {
        let r = AudioRing::new(1000);
        // 没人读的情况下灌进 1 秒数据（每次 100 个）
        for batch in 0..10 {
            let data: Vec<f32> = (0..100).map(|i| (batch * 100 + i) as f32).collect();
            r.trim(50);
            r.push(&data);
        }
        let (avail, dropped, _) = r.stats();
        assert!(avail <= 150, "裁剪后占用应有界，实际 {avail}");
        assert_eq!(dropped, 0, "主动裁剪不应计入丢弃（那是我们有意为之）");
        assert!(r.trimmed() > 0, "裁剪量必须被统计，否则丢样会隐形");
        // 读出来的必须是最后一段（最新）音频
        let mut out = vec![0f32; avail];
        let got = r.pop(&mut out);
        assert_eq!(got, avail);
        assert_eq!(out[avail - 1], 999.0, "最新样本必须是 999");
        assert!(out[0] >= 850.0, "留下的应是末尾这段，实际首样本 {}", out[0]);
    }
}
