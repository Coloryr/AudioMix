//! 采集线程 → 混音线程之间的无锁 SPSC 环形缓冲（每条路由一条）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// 环形缓冲的写端（`ringbuf` 类型别名）。
pub type Prod = HeapProd<f32>;
/// 环形缓冲的读端（`ringbuf` 类型别名）。
pub type Cons = HeapCons<f32>;

/// 路由边（source → sink）的共享缓冲写端。
/// Producer 端由采集线程持有，Consumer 端由对应 sink 的混音线程持有；
/// 两侧各自用 Mutex 包装（各自单线程访问，无争用）。
pub struct EdgeWriter {
    pub producer: Mutex<Prod>,
    /// 缓冲满时被丢弃的样本累计数（采集回调不能阻塞，只能丢）
    pub dropped: AtomicU64,
}

/// 路由边的读端，由对应 sink 的渲染线程持有。
pub struct EdgeReader {
    pub consumer: Mutex<Cons>,
}

/// 一条路由边的读写两端（共享同一块堆内存）。
pub struct EdgeRing {
    pub writer: Arc<EdgeWriter>,
    pub reader: Arc<EdgeReader>,
}

/// 创建一条边缓冲。`frames_capacity` 为帧数容量（内部乘以通道数）。
pub fn new_edge_ring(frames_capacity: usize, channels: usize) -> EdgeRing {
    let (prod, cons) = HeapRb::<f32>::new(frames_capacity.max(1) * channels.max(1)).split();
    EdgeRing {
        writer: Arc::new(EdgeWriter {
            producer: Mutex::new(prod),
            dropped: AtomicU64::new(0),
        }),
        reader: Arc::new(EdgeReader {
            consumer: Mutex::new(cons),
        }),
    }
}

impl EdgeWriter {
    /// 推入 interleaved 帧；缓冲满则丢弃（采集回调不能阻塞）。
    pub fn push(&self, data: &[f32]) {
        let mut prod = self.producer.lock();
        let pushed = prod.push_slice(data);
        if pushed < data.len() {
            self.dropped
                .fetch_add((data.len() - pushed) as u64, Ordering::Relaxed);
        }
    }

    /// 累计丢弃的样本数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl EdgeReader {
    /// 取出当前全部可用样本（最多 `max_samples`），追加到 out。
    pub fn drain(&self, max_samples: usize, out: &mut Vec<f32>) {
        let mut cons = self.consumer.lock();
        let avail = cons.occupied_len().min(max_samples);
        let start = out.len();
        out.resize(start + avail, 0.0);
        let got = cons.pop_slice(&mut out[start..]);
        out.truncate(start + got);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_roundtrip() {
        let ring = new_edge_ring(16, 2);
        ring.writer.push(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = Vec::new();
        ring.reader.drain(1024, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn test_ring_overflow_drops() {
        let ring = new_edge_ring(2, 1); // 2 样本容量
        ring.writer.push(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.writer.dropped(), 2);
        let mut out = Vec::new();
        ring.reader.drain(1024, &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
    }
}
