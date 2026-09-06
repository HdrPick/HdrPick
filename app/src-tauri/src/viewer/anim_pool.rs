//! JXL 动图 GPU 纹理池（生产者-消费者槽位模型）
//!
//! 滑动窗口语义与 [`super::hdr_viewer::AnimRing`] 流式语义同构：
//! - 全局单调 frame_idx（跨循环不回退），容量 = 显存预算/帧字节（调用方钳制后传入）
//! - 淘汰 = 消费归还（无主动淘汰、无重绕协议）；填充线程无空闲槽位时阻塞等待
//!
//! # 核心正确性设计：在屏纹理绝不被复用
//! [`GpuFramePool::get`] 命中就绪帧时把纹理从槽位**独占取出**（RAII guard
//! [`GpuFrame`] 持有），槽位脱离 free 列表；guard Drop（窗口线程换帧）时纹理
//! 归还槽位、槽位重回 free——此刻该纹理已不在屏，填充线程才可复用其显存重填。
//! 在屏期间槽位对生产者不可见，杜绝写入竞态。
//!
//! # 可测试性
//! D3D 对象与簿记逻辑分离：纹理句柄负载泛型化为 `T: Clone + Send`，真实实现
//! 用 type alias [`AnimFramePool`] = `GpuFramePool<PoolTex>`（factory 注入
//! [`d3d_factory`] 产物），测试注入 mock（T=usize）纯逻辑验证簿记。
//!
//! # 槽位规格
//! RGBA16F Texture2D（w×h，MipLevels=1，ArraySize=1，Usage=DEFAULT，
//! BindFlags=SHADER_RESOURCE|UNORDERED_ACCESS），SRV + UAV 各一。

use std::sync::{Arc, Condvar, Mutex};

use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_TEX2D_UAV,
    D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};

// ==================== 纹理负载（真实 D3D 实现） ====================

/// 池槽位纹理负载：一张 RGBA16F 纹理 + 两个视图
///
/// ID3D11 接口 Clone = COM AddRef，跨线程传递安全。
#[derive(Clone)]
pub struct PoolTex {
    pub tex: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,
    pub uav: ID3D11UnorderedAccessView,
}

/// 真实 D3D 版纹理池
pub type AnimFramePool = GpuFramePool<PoolTex>;

/// 构造真实 D3D 工厂闭包：按槽位规格创建 PoolTex（注入 [`GpuFramePool::new`]）
///
/// dims 与池一致（调用方保证）；预分配在 new() 内同步完成，
/// 分配中途失败时已建纹理随本地 Vec Drop（COM Release）归还显存。
pub fn d3d_factory(
    device: ID3D11Device,
    dims: (u32, u32),
) -> impl FnMut() -> Result<PoolTex, String> + Send {
    move || {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: dims.0,
            Height: dims.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| format!("池纹理 CreateTexture2D 失败: {}", e))?;
        let tex: ID3D11Texture2D = tex.ok_or_else(|| "池纹理创建返回空".to_string())?;

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 },
            },
        };
        let mut srv = None;
        unsafe { device.CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv)) }
            .map_err(|e| format!("池纹理 SRV 创建失败: {}", e))?;
        let srv: ID3D11ShaderResourceView = srv.ok_or_else(|| "池纹理 SRV 返回空".to_string())?;

        let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
            },
        };
        let mut uav = None;
        unsafe { device.CreateUnorderedAccessView(&tex, Some(&uav_desc), Some(&mut uav)) }
            .map_err(|e| format!("池纹理 UAV 创建失败: {}", e))?;
        let uav: ID3D11UnorderedAccessView = uav.ok_or_else(|| "池纹理 UAV 返回空".to_string())?;

        Ok(PoolTex { tex, srv, uav })
    }
}

// ==================== 簿记结构（与 D3D 解耦，纯逻辑可测） ====================

/// 单个槽位：一张纹理 + 其当前承载的帧信息
pub struct PoolSlot<T> {
    /// 未发布时为 u64::MAX 哨兵（ready=false 兜底，永不误匹配）
    pub frame_idx: u64,
    pub duration_ms: u32,
    pub ready: bool,
    pub tex: Option<T>,
}

/// 池运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolState {
    /// 填充中（首帧已就绪前 / 未达全驻留）
    Filling,
    /// 全驻留：filled_frames == total_frames（total 已知）
    FullResident,
    /// 降级（GPU 预算不足等，调用方标记；后续 publish 不再覆盖回 FullResident）
    Degraded,
}

/// 统计快照
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    pub filled_frames: u64,
    pub filled_ms: u64,
    pub capacity: usize,
    pub state: PoolState,
}

/// Mutex 内簿记（factory 不驻留：仅 new() 预分配期使用，用完即弃）
struct PoolInner<T> {
    slots: Vec<PoolSlot<T>>,
    /// 空闲槽位栈（LIFO 复用，局部性更好）
    free: Vec<usize>,
    filled_frames: u64,
    total_frames: Option<u64>,
    filled_ms: u64,
    state: PoolState,
    closed: bool,
    /// 生产者饥饿标记：填充线程阻塞在 acquire_free_wait 等待空闲槽。
    /// 播放端未命中时据此区分"填充饥饿（须归还 guard 解锁）"与
    /// "填充持槽解码中（等待即可）"——后者贸然归还会让槽位被复用改写。
    producer_waiting: bool,
    dims: (u32, u32),
    frame_bytes: u64,
}

// ==================== 池本体 ====================

/// JXL 动图 GPU 帧纹理池（`Arc<GpuFramePool<T>>` 共享）
///
/// 生产者（填充线程）：`acquire_free`/`acquire_free_wait` 取空闲槽 → 解码写入
/// 纹理 → `publish` 标记就绪；消费者（窗口线程）：`get` 独占取帧绘制，
/// guard Drop 自动归还。`close()` 唤醒一切等待者。
pub struct GpuFramePool<T: Clone + Send> {
    inner: Mutex<PoolInner<T>>,
    cv: Condvar,
}

impl<T: Clone + Send> GpuFramePool<T> {
    /// 创建池并**立即预分配**全部 capacity 张纹理（预算钳制由调用方算好传入）
    ///
    /// 分配中途失败 → 已建纹理随本地 Vec Drop 释放（真实 PoolTex 即 COM
    /// Release 归还显存），返回 Err。
    pub fn new(
        dims: (u32, u32),
        capacity: usize,
        mut factory: impl FnMut() -> Result<T, String>,
    ) -> Result<Arc<Self>, String> {
        if dims.0 == 0 || dims.1 == 0 {
            return Err(format!("纹理池尺寸非法: {:?}", dims));
        }
        if capacity == 0 {
            return Err("纹理池容量必须 > 0".to_string());
        }
        let mut created: Vec<T> = Vec::with_capacity(capacity);
        for i in 0..capacity {
            match factory() {
                Ok(t) => created.push(t),
                Err(e) => {
                    // created 在此 Drop：真实 PoolTex 析构 → 显存归还
                    return Err(format!(
                        "纹理池预分配失败（{}/{} 张已建）: {}",
                        i, capacity, e
                    ));
                }
            }
        }
        let frame_bytes = dims.0 as u64 * dims.1 as u64 * 8; // RGBA16F = 8 B/px
        let mut slots = Vec::with_capacity(capacity);
        let mut free = Vec::with_capacity(capacity);
        for (i, tex) in created.into_iter().enumerate() {
            slots.push(PoolSlot {
                frame_idx: u64::MAX,
                duration_ms: 0,
                ready: false,
                tex: Some(tex),
            });
            free.push(i);
        }
        Ok(Arc::new(Self {
            inner: Mutex::new(PoolInner {
                slots,
                free,
                filled_frames: 0,
                total_frames: None,
                filled_ms: 0,
                state: PoolState::Filling,
                closed: false,
                producer_waiting: false,
                dims,
                frame_bytes,
            }),
            cv: Condvar::new(),
        }))
    }

    /// 非阻塞取空闲槽 idx；无则 None
    pub fn acquire_free(&self) -> Option<usize> {
        self.inner.lock().unwrap().free.pop()
    }

    /// 阻塞等待空闲槽或 closed（closed → None）——填充线程用
    ///
    /// 阻塞期间置 producer_waiting（饥饿标记，消费端可查），取到槽即清除。
    pub fn acquire_free_wait(&self) -> Option<usize> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.closed {
                return None;
            }
            if let Some(idx) = g.free.pop() {
                g.producer_waiting = false;
                return Some(idx);
            }
            g.producer_waiting = true;
            g = self.cv.wait(g).unwrap();
        }
    }

    /// 填充线程是否饥饿（正阻塞等待空闲槽）——播放端帧未命中时的归还依据：
    /// true → 归还最旧 guard 解锁填充；false → 填充持槽解码中，等待即可。
    pub fn producer_starved(&self) -> bool {
        self.inner.lock().unwrap().producer_waiting
    }

    /// 标记槽 idx 帧就绪 + 记账（filled_frames/filled_ms）
    ///
    /// 池不校验帧序（全局单调 frame_idx 由调用方保证）；帧序必须等于该槽
    /// 上一次消费的期望序这一约束同样由调用方承担。
    pub fn publish(&self, idx: usize, frame_idx: u64, duration_ms: u32) {
        let mut g = self.inner.lock().unwrap();
        debug_assert!(idx < g.slots.len(), "publish 槽位越界: {}", idx);
        if let Some(slot) = g.slots.get_mut(idx) {
            slot.frame_idx = frame_idx;
            slot.duration_ms = duration_ms;
            slot.ready = true;
            g.filled_frames += 1;
            g.filled_ms += duration_ms as u64;
            // 全驻留判定：Degraded 一经标记不被覆盖
            if g.state == PoolState::Filling {
                if let Some(total) = g.total_frames {
                    if g.filled_frames >= total {
                        g.state = PoolState::FullResident;
                    }
                }
            }
        }
        drop(g);
        self.cv.notify_all();
    }

    /// 阻塞直到该 frame_idx 就绪或 closed（仅首帧同步用；返回是否就绪）
    pub fn wait_frame(&self, frame_idx: u64) -> bool {
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.slots.iter().any(|s| s.ready && s.frame_idx == frame_idx) {
                return true;
            }
            if g.closed {
                return false;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    /// 帧就绪则独占取出（RAII guard [`GpuFrame`]），未就绪 → None
    ///
    /// 命中时纹理从槽位摘出（在屏），guard Drop 自动归还槽到 free——
    /// 在屏纹理在归还前对生产者不可见，绝不被复用。
    pub fn get(self: &Arc<Self>, frame_idx: u64) -> Option<GpuFrame<T>> {
        let mut g = self.inner.lock().unwrap();
        for idx in 0..g.slots.len() {
            if g.slots[idx].ready && g.slots[idx].frame_idx == frame_idx {
                if let Some(tex) = g.slots[idx].tex.take() {
                    let duration_ms = g.slots[idx].duration_ms;
                    g.slots[idx].ready = false;
                    return Some(GpuFrame {
                        pool: Arc::clone(self),
                        idx,
                        tex: Some(tex),
                        frame_idx,
                        duration_ms,
                    });
                }
                return None; // 不变式上不可达（ready 槽必有 tex）
            }
        }
        None
    }

    /// 槽位纹理句柄（COM clone = AddRef；填充线程 GPU 直写用，槽位所有权不动）
    ///
    /// 填充线程 `acquire_free_wait` 拿到槽 idx 后取此句柄，把 GPU 链输出
    /// 直写进同一张纹理（写入通过 COM 引用即达本体），随后 `publish` 标记
    /// 就绪——纹理始终留在槽内，无取放协议。
    pub fn slot_tex(&self, idx: usize) -> Option<T> {
        let g = self.inner.lock().unwrap();
        g.slots.get(idx).and_then(|s| s.tex.clone())
    }

    /// 动图总帧数（None = 未知/未定）
    pub fn total_frames(&self) -> Option<u64> {
        self.inner.lock().unwrap().total_frames
    }

    /// 设置总帧数；若已填充达量立即转 FullResident
    pub fn set_total(&self, total: u64) {
        let mut g = self.inner.lock().unwrap();
        g.total_frames = Some(total);
        if g.state == PoolState::Filling && g.filled_frames >= total {
            g.state = PoolState::FullResident;
        }
    }

    /// 标记降级（后续 publish 不再覆盖回 FullResident）
    pub fn set_degraded(&self) {
        self.inner.lock().unwrap().state = PoolState::Degraded;
    }

    /// 统计快照
    pub fn stats(&self) -> PoolStats {
        let g = self.inner.lock().unwrap();
        PoolStats {
            filled_frames: g.filled_frames,
            filled_ms: g.filled_ms,
            capacity: g.slots.len(),
            state: g.state,
        }
    }

    /// 池尺寸（pixels）
    pub fn dims(&self) -> (u32, u32) {
        self.inner.lock().unwrap().dims
    }

    /// 池容量（槽位数）——全驻留判定 total ≤ capacity 用
    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap().slots.len()
    }

    /// 单帧字节数（RGBA16F：w×h×8）
    pub fn frame_bytes(&self) -> u64 {
        self.inner.lock().unwrap().frame_bytes
    }

    /// 关闭池：唤醒一切等待者（acquire_free_wait → None / wait_frame → false）
    ///
    /// 已就绪未消费的帧仍可 get（排空语义由调用方决定）。
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.cv.notify_all();
    }

    /// 阻塞至池关闭——全驻留挂起的填充线程专用（close() 唤醒返回）。
    ///
    /// 与 acquire_free_wait 不同：不消费空闲槽（全驻留下 total < capacity 时
    /// 池内恒有余槽，acquire_free_wait 会立即返回无法挂起）。
    pub fn wait_closed(&self) {
        let mut g = self.inner.lock().unwrap();
        while !g.closed {
            g = self.cv.wait(g).unwrap();
        }
    }
}

// ==================== RAII 在屏 guard ====================

/// 在屏帧 guard：`get` 返回；Drop 时纹理归还槽位、槽位重回 free
///
/// 持有期间该槽位对生产者不可见（在屏纹理不复用的保证）；Drop = 窗口线程
/// 换帧 release 上一帧的时刻。
pub struct GpuFrame<T: Clone + Send> {
    pool: Arc<GpuFramePool<T>>,
    idx: usize,
    /// Option 化便于 Drop 时把所有权移回槽位（None 仅存在于归还瞬间）
    tex: Option<T>,
    pub frame_idx: u64,
    pub duration_ms: u32,
}

impl<T: Clone + Send> GpuFrame<T> {
    /// 在屏纹理句柄（SRV 供窗口绘制 / UAV 备用）
    pub fn tex(&self) -> &T {
        self.tex.as_ref().expect("guard 存活期间纹理必然在持")
    }

    /// 槽位下标（内部簿记用）
    pub fn slot(&self) -> usize {
        self.idx
    }
}

impl<T: Clone + Send> Drop for GpuFrame<T> {
    fn drop(&mut self) {
        let mut g = self.pool.inner.lock().unwrap();
        // Drop = 换帧：纹理已不在屏，归还原槽（显存复用），槽位重回 free
        g.slots[self.idx].tex = self.tex.take();
        g.slots[self.idx].ready = false;
        g.slots[self.idx].frame_idx = u64::MAX;
        g.free.push(self.idx);
        drop(g);
        self.pool.cv.notify_all();
    }
}

// ==================== 单元测试（纯逻辑 mock：T = usize） ====================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// mock 池：factory 返回自增序号，同时计数分配次数
    fn mock_pool(capacity: usize) -> (Arc<GpuFramePool<usize>>, Arc<AtomicUsize>) {
        let allocs = Arc::new(AtomicUsize::new(0));
        let a2 = allocs.clone();
        let pool = GpuFramePool::new((8, 8), capacity, move || {
            Ok(a2.fetch_add(1, Ordering::SeqCst))
        })
        .unwrap();
        (pool, allocs)
    }

    #[test]
    fn test_new_rejects_invalid() {
        assert!(GpuFramePool::<usize>::new((0, 8), 1, || Ok(1)).is_err());
        assert!(GpuFramePool::<usize>::new((8, 0), 1, || Ok(1)).is_err());
        assert!(GpuFramePool::<usize>::new((8, 8), 0, || Ok(1)).is_err());
    }

    #[test]
    fn test_factory_failure_drops_created() {
        let allocs = Arc::new(AtomicUsize::new(0));
        let a2 = allocs.clone();
        let r = GpuFramePool::<usize>::new((8, 8), 3, move || {
            let n = a2.fetch_add(1, Ordering::SeqCst) + 1;
            if n >= 3 {
                Err("设备创建失败".to_string())
            } else {
                Ok(n)
            }
        });
        assert!(r.is_err());
        let e = match r {
            Err(e) => e,
            Ok(_) => panic!("工厂中途失败应返回 Err"),
        };
        assert!(e.contains("2/3"), "错误信息应含已建/总数，实际: {}", e);
        assert_eq!(allocs.load(Ordering::SeqCst), 3, "第 3 次调用仍触发 factory");
        // 已建 2 张随 Err 路径 Drop（真实实现即 COM Release）
    }

    #[test]
    fn test_capacity3_recycle() {
        let (pool, allocs) = mock_pool(3);
        assert_eq!(allocs.load(Ordering::SeqCst), 3, "预分配恰好 capacity 张");
        assert_eq!(pool.frame_bytes(), 8 * 8 * 8, "RGBA16F 帧字节 = w*h*8");

        for f in 0..3u64 {
            let idx = pool.acquire_free().expect("有空闲槽");
            pool.publish(idx, f, 16);
        }
        assert!(pool.acquire_free().is_none(), "满载无空闲槽");
        assert_eq!(pool.stats().state, PoolState::Filling, "total 未知 → 不判定全驻留");

        let g = pool.get(0).expect("帧 0 已就绪");
        assert_eq!(g.frame_idx, 0);
        assert_eq!(g.duration_ms, 16);
        // 在屏期间：槽位不归还、同帧不可二次取出
        assert!(pool.acquire_free().is_none(), "在屏纹理不归还");
        assert!(pool.get(0).is_none(), "在屏帧不可重复 get");

        drop(g); // 换帧归还 → 空闲槽出现
        let idx = pool.acquire_free().expect("归还后有空闲槽");
        assert_eq!(allocs.load(Ordering::SeqCst), 3, "复用：零新分配");
        pool.publish(idx, 3, 16);
        assert!(pool.get(3).is_some());
        assert_eq!(pool.stats().filled_frames, 4);
        assert_eq!(pool.stats().filled_ms, 64);
    }

    #[test]
    fn test_backpressure_wait_wakeup() {
        let (pool, _a) = mock_pool(2);
        for f in 0..2u64 {
            let idx = pool.acquire_free().unwrap();
            pool.publish(idx, f, 16);
        }
        // 满 + 未消费 → 等待者阻塞
        let p2 = pool.clone();
        let waiter = std::thread::spawn(move || p2.acquire_free_wait());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "满载未消费时 acquire_free_wait 应阻塞");

        // 消费一帧 → guard drop → 等待者被唤醒拿到归还槽
        drop(pool.get(0).expect("帧 0"));
        let idx = waiter.join().unwrap().expect("归还后唤醒");
        pool.publish(idx, 2, 16);
        assert!(pool.get(2).is_some());
    }

    #[test]
    fn test_get_not_ready_and_wait_frame() {
        let (pool, _a) = mock_pool(2);
        assert!(pool.get(0).is_none(), "未发布帧不可取");

        // 阻塞等首帧 → 发布后唤醒
        let p2 = pool.clone();
        let h = std::thread::spawn(move || p2.wait_frame(0));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!h.is_finished(), "wait_frame 应阻塞至发布");
        let idx = pool.acquire_free().unwrap();
        pool.publish(idx, 0, 16);
        assert!(h.join().unwrap(), "发布后唤醒 → true");
        assert!(pool.get(0).is_some());
    }

    #[test]
    fn test_close_wakes_all() {
        let (pool, _a) = mock_pool(1);
        let idx = pool.acquire_free().unwrap();
        pool.publish(idx, 0, 16);

        // 满 + 未消费 → 阻塞 → close 唤醒 → None
        let p2 = pool.clone();
        let h1 = std::thread::spawn(move || p2.acquire_free_wait());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!h1.is_finished());
        pool.close();
        assert!(h1.join().unwrap().is_none(), "close 唤醒 acquire_free_wait → None");

        // closed 后 wait_frame 立即 false（含未发布帧）
        let p3 = pool.clone();
        let h2 = std::thread::spawn(move || p3.wait_frame(1));
        assert!(!h2.join().unwrap());
        assert!(!pool.wait_frame(1));
    }

    #[test]
    fn test_full_resident_and_degraded() {
        let (pool, _a) = mock_pool(3);
        assert_eq!(pool.stats().state, PoolState::Filling);
        assert_eq!(pool.total_frames(), None);

        // 路径 A：填满后 set_total → 立即全驻留
        for f in 0..3u64 {
            let idx = pool.acquire_free().unwrap();
            pool.publish(idx, f, 16);
        }
        pool.set_total(3);
        let s = pool.stats();
        assert_eq!(s.state, PoolState::FullResident);
        assert_eq!(s.filled_frames, 3);
        assert_eq!(s.filled_ms, 48);
        assert_eq!(s.capacity, 3);
        assert_eq!(pool.total_frames(), Some(3));

        // Degraded 后 publish 不再拉回 FullResident
        pool.set_degraded();
        assert_eq!(pool.stats().state, PoolState::Degraded);
        drop(pool.get(0).expect("帧 0"));
        let idx = pool.acquire_free().unwrap();
        pool.publish(idx, 3, 16);
        assert_eq!(pool.stats().state, PoolState::Degraded);

        // 路径 B：先 set_total 再填满 → publish 时判定全驻留
        let (pool2, _a2) = mock_pool(2);
        pool2.set_total(2);
        assert_eq!(pool2.stats().state, PoolState::Filling);
        for f in 0..2u64 {
            let idx = pool2.acquire_free().unwrap();
            pool2.publish(idx, f, 16);
        }
        assert_eq!(pool2.stats().state, PoolState::FullResident);
    }

    #[test]
    fn test_stress_2_producers_1_consumer_1000_frames() {
        let (pool, allocs) = mock_pool(3);
        const TOTAL: u64 = 1000;
        pool.set_total(TOTAL);
        let next = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::new();
        // 2 填充线程：先取空闲槽（满则阻塞）→ 认领帧号 → 模拟解码 → 发布
        for _ in 0..2 {
            let p = pool.clone();
            let next = next.clone();
            handles.push(std::thread::spawn(move || loop {
                // 先占槽再认领帧号：认领者必然持槽，保证认领的帧最终必被发布
                let Some(idx) = p.acquire_free_wait() else { break };
                let f = next.fetch_add(1, Ordering::SeqCst);
                if f >= TOTAL {
                    break;
                }
                std::thread::sleep(Duration::from_micros(20));
                p.publish(idx, f, 16);
            }));
        }
        // 1 消费线程：按序等帧 → 独占取出 → 立即归还
        let p = pool.clone();
        handles.push(std::thread::spawn(move || {
            for f in 0..TOTAL {
                assert!(p.wait_frame(f), "帧 {} 等待失败", f);
                let g = p
                    .get(f)
                    .unwrap_or_else(|| panic!("帧 {} 已就绪却取不到", f));
                assert_eq!(g.frame_idx, f);
                assert_eq!(g.duration_ms, 16);
                drop(g);
            }
        }));
        for h in handles {
            h.join().unwrap();
        }

        let s = pool.stats();
        assert_eq!(s.filled_frames, TOTAL);
        assert_eq!(s.filled_ms, TOTAL * 16);
        assert_eq!(s.state, PoolState::FullResident);
        assert!(
            allocs.load(Ordering::SeqCst) <= 3,
            "复用验证：1000 帧分配次数不得超过 capacity"
        );
    }

    // ==================== B7：滑动窗口 / 小预算专项 ====================

    /// 滑动窗口流转模拟：单填充线程按序发布 [start, end)，单消费线程按序
    /// get（未就绪立即重试 + 短 sleep 模拟播放节拍）。返回消费端未命中重试次数。
    ///
    /// 带看门狗：超过时限判疑似死锁 → abort + close 唤醒一切等待者后 panic；
    /// 消费端断言 panic 则原样传播（避免被超时路径掩盖真实原因）。
    fn run_sliding_window(
        pool: &Arc<GpuFramePool<usize>>,
        start: u64,
        end: u64,
        producer_delay: Duration,
        consumer_retry: Duration,
    ) -> usize {
        let abort = Arc::new(AtomicBool::new(false));
        let p = pool.clone();
        let producer = std::thread::spawn(move || {
            for f in start..end {
                // 看门狗超时路径 close() → None → 退出
                let Some(idx) = p.acquire_free_wait() else { return };
                std::thread::sleep(producer_delay); // 模拟解码写入节拍
                p.publish(idx, f, 16);
            }
        });
        let p = pool.clone();
        let abort2 = abort.clone();
        let consumer = std::thread::spawn(move || {
            let mut misses = 0usize;
            for f in start..end {
                let g = loop {
                    if let Some(g) = p.get(f) {
                        break g;
                    }
                    if abort2.load(Ordering::SeqCst) {
                        return misses; // 看门狗中止（随后必 panic，不计入断言）
                    }
                    misses += 1;
                    std::thread::sleep(consumer_retry);
                };
                assert_eq!(g.frame_idx, f, "必须全序消费");
                assert_eq!(g.duration_ms, 16);
                drop(g); // 换帧归还 → 驱动填充前进（背压释放）
                // 已消费帧（归还后 frame_idx 复位）不可重复可取
                assert!(p.get(f).is_none(), "帧 {} 消费归还后不可重复 get", f);
            }
            misses
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        let consumer_result = loop {
            if consumer.is_finished() {
                break consumer.join();
            }
            if Instant::now() > deadline {
                abort.store(true, Ordering::SeqCst);
                pool.close();
                let _ = producer.join();
                let _ = consumer.join();
                panic!("滑动窗口流转超时（{} 帧未在 30s 内完成），疑似死锁", end - start);
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let misses = match consumer_result {
            Ok(m) => m,
            Err(e) => {
                // 消费端断言失败：先唤醒/回收填充线程再原样传播 panic
                pool.close();
                let _ = producer.join();
                std::panic::resume_unwind(e);
            }
        };
        producer.join().unwrap();
        misses
    }

    /// B7-1：capacity=15（512MB 预算量级），314 帧长动画滑动窗口循环 2 轮。
    ///
    /// 628 次 get/publish 流转：填充被背压限制（≤15 帧超前），消费归还驱动前进。
    /// 断言：全序消费 0..628、工厂分配计数恒 15、无死锁（看门狗）、filled_frames
    /// 累计记账正确。
    #[test]
    fn test_sliding_window_small_budget() {
        let (pool, allocs) = mock_pool(15);
        assert_eq!(allocs.load(Ordering::SeqCst), 15, "预分配恰好 capacity 张");
        const TOTAL: u64 = 314;
        const ROUNDS: u64 = 2;
        pool.set_total(TOTAL);

        let _misses = run_sliding_window(
            &pool,
            0,
            TOTAL * ROUNDS,
            Duration::from_micros(10),
            Duration::from_micros(50),
        );

        let s = pool.stats();
        assert_eq!(s.filled_frames, TOTAL * ROUNDS, "628 次 publish 累计记账");
        assert_eq!(s.filled_ms, TOTAL * ROUNDS * 16);
        assert_eq!(s.capacity, 15);
        // 既有簿记语义（与 stress 测试一致）：filled_frames 累计达 total →
        // FullResident。滑动窗口下这仅表示"首轮发布完成"，上层若据此刻停
        // 填充线程会使第二轮饿死——须以 total ≤ capacity 判定，不得读此状态。
        assert_eq!(s.state, PoolState::FullResident);
        assert_eq!(allocs.load(Ordering::SeqCst), 15, "工厂分配计数恒 15（零新分配）");
    }

    /// B7-2：播放前沿越过填充前沿（消费快于填充）时窗口语义不破坏：
    /// 未就绪 → None（上层"保持当前帧"的池层表现）；填充追上后立即可取；
    /// 已消费帧（guard 持有或归还后）同 idx 二次 get 均 → None。
    #[test]
    fn test_leading_edge_hold() {
        let (pool, allocs) = mock_pool(4);

        // 确定性阶段：前沿未填充 → None；就绪后立即可取；消费后不可重复取
        assert!(pool.get(0).is_none(), "填充前沿未到 → None");
        let idx = pool.acquire_free().unwrap();
        pool.publish(idx, 0, 16);
        let g0 = pool.get(0).expect("填充追上后立即可取");
        assert!(pool.get(0).is_none(), "在屏（guard 持有）期间同帧不可二次 get");
        drop(g0);
        assert!(pool.get(0).is_none(), "已消费帧归还后不可重复可取");

        // 线程阶段：填充每帧 1ms 慢于消费 50µs 重试 → 连续未命中必 > 0，
        // 且一旦填充追上每帧都取到（run_sliding_window 内断言全序）
        let misses = run_sliding_window(
            &pool,
            1,
            12,
            Duration::from_millis(1),
            Duration::from_micros(50),
        );
        assert!(misses > 0, "消费快于填充必然出现未就绪重试，实际 {}", misses);
        assert_eq!(allocs.load(Ordering::SeqCst), 4, "零新分配");
        assert_eq!(pool.stats().filled_frames, 12);
    }

    /// B7-3：total=10 ≤ capacity=15 → 全驻留；3 轮循环（全局 frame_idx 单调，
    /// 第 r 轮 = [r*10, (r+1)*10)）每次 get 都命中、零新分配。
    #[test]
    fn test_short_animation_full_resident_zero_alloc() {
        let (pool, allocs) = mock_pool(15);
        assert_eq!(
            allocs.load(Ordering::SeqCst),
            15,
            "预分配 = capacity（上层可用 min(预算帧数, total) 钳制，见末尾子断言）"
        );
        const TOTAL: u64 = 10;
        pool.set_total(TOTAL);

        // 首轮填充 0..10 → 填满即 FullResident
        let mut guard = None;
        for f in 0..TOTAL {
            let idx = pool.acquire_free().expect("capacity 15 > total 10，取槽无需等待");
            pool.publish(idx, f, 16);
            guard = Some(pool.get(f).expect("全驻留路径每次 get 都命中"));
        }
        assert_eq!(pool.stats().state, PoolState::FullResident, "填满后全驻留");

        // 3 轮循环（含首轮）：消费归还驱动槽位复用，全局 idx 单调递增
        for r in 1..3u64 {
            for k in 0..TOTAL {
                let f = r * TOTAL + k;
                drop(guard.take().unwrap()); // 换帧归还
                let idx = pool.acquire_free().expect("消费归还后有槽");
                pool.publish(idx, f, 16);
                guard = Some(pool.get(f).expect("循环帧每次 get 都命中"));
            }
        }
        drop(guard);

        let s = pool.stats();
        assert_eq!(s.filled_frames, 3 * TOTAL, "3 轮 × 10 帧 publish 记账");
        assert_eq!(s.filled_ms, 3 * TOTAL * 16);
        assert_eq!(s.state, PoolState::FullResident);
        assert_eq!(allocs.load(Ordering::SeqCst), 15, "循环零新分配（工厂计数不增）");

        // 上层按 min(预算帧数, total) 钳制 capacity 的路径：工厂计数恒 = total
        let (p2, a2) = mock_pool(TOTAL as usize);
        assert_eq!(a2.load(Ordering::SeqCst), 10, "钳制 capacity=total → 工厂计数恒 10");
        p2.set_total(TOTAL);
        for f in 0..TOTAL {
            let idx = p2.acquire_free().unwrap();
            p2.publish(idx, f, 16);
        }
        assert_eq!(p2.stats().state, PoolState::FullResident);
        drop(p2.get(0).unwrap());
        let idx = p2.acquire_free().unwrap();
        p2.publish(idx, TOTAL, 16);
        assert!(p2.get(TOTAL).is_some(), "循环第二遍帧立即填上");
        assert_eq!(a2.load(Ordering::SeqCst), 10, "循环零新分配");
    }

    /// B7-4：capacity=20 ≥ total=15：第二轮直接发布全局连续 idx 15..29，
    /// 旧帧 0..14 全程不消费也不阻塞新帧发布（acquire_free 非阻塞成功），
    /// 验证全局 idx 单调 + "帧已在池或立即填上"（第二轮无需等待）。
    #[test]
    fn test_loop_second_round_prefetch() {
        let (pool, allocs) = mock_pool(20);
        assert_eq!(allocs.load(Ordering::SeqCst), 20);
        const TOTAL: u64 = 15;
        pool.set_total(TOTAL);

        // 第一轮：发布 0..14，故意全部不消费（旧帧驻留占槽）
        for f in 0..TOTAL {
            let idx = pool.acquire_free().unwrap();
            pool.publish(idx, f, 16);
        }
        assert_eq!(pool.stats().state, PoolState::FullResident);

        // 第二轮：15 空闲槽余量 + 逐帧归还上一新帧 → acquire_free 全程非阻塞
        let mut guard = None;
        for f in TOTAL..2 * TOTAL {
            drop(guard.take()); // 换帧归还上一新帧（旧帧不动）
            let idx = pool.acquire_free().expect(
                "capacity ≥ total：第二轮取槽无需等待（旧帧未消费也不影响新帧发布）",
            );
            pool.publish(idx, f, 16);
            let g = pool.get(f).expect("发布后立即命中（帧已在池或立即填上）");
            assert_eq!(g.frame_idx, f, "全局 frame_idx 单调递增");
            guard = Some(g);
        }
        drop(guard);

        let s = pool.stats();
        assert_eq!(s.filled_frames, 2 * TOTAL, "30 次 publish 记账");
        assert_eq!(s.state, PoolState::FullResident);
        assert_eq!(allocs.load(Ordering::SeqCst), 20, "零新分配");
        // 旧帧 0..14 从未被消费，仍驻留池中（未被第二轮驱逐）
        assert!(pool.get(0).is_some(), "旧头帧仍在池");
        assert!(pool.get(TOTAL - 1).is_some(), "旧尾帧仍在池");
    }

    /// B7-5（极端）：最小容量 capacity=2 跑 50 帧——最小容量下流转仍须
    /// 全序、背压正常（消费必等待填充）、零新分配、无死锁。
    #[test]
    fn test_minimum_capacity_two_50_frames() {
        let (pool, allocs) = mock_pool(2);
        assert_eq!(allocs.load(Ordering::SeqCst), 2);
        const FRAMES: u64 = 50;
        pool.set_total(FRAMES);

        let misses = run_sliding_window(
            &pool,
            0,
            FRAMES,
            Duration::from_micros(100),
            Duration::from_micros(50),
        );
        assert!(misses > 0, "容量 2 时消费必然出现未就绪等待，实际 {}", misses);

        let s = pool.stats();
        assert_eq!(s.filled_frames, FRAMES);
        assert_eq!(s.filled_ms, FRAMES * 16);
        assert_eq!(s.state, PoolState::FullResident);
        assert_eq!(allocs.load(Ordering::SeqCst), 2, "工厂分配计数恒 2（零新分配）");
    }
}
