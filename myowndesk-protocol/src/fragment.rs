/// 处理超过 MTU 的视频编码数据分片。
///
/// 默认策略：编码器 slice 模式确保每个 NAL unit 不超过单个 datagram。
/// 当 NAL unit 超过 MTU 时（少数硬件），通过此 trait 进行分片。
pub trait FrameFragmenter: Send + Sync {
    /// 将编码数据按 MTU 大小拆分为片段。
    fn fragment(&self, data: &[u8], mtu: usize) -> Vec<Vec<u8>>;

    /// 尝试将收到的片段重组为完整帧。
    /// 返回 `Some(完整帧)` 表示所有片段已收集齐，
    /// 返回 `None` 表示还有片段未到达。
    fn reassemble(
        &self,
        frame_seq: u32,
        fragment_index: u32,
        fragment_count: u32,
        data: &[u8],
    ) -> Option<Vec<u8>>;
}

/// 无分片实现：假设所有 NAL unit 均不超过 MTU。
///
/// 这是编码器 slice 策略的默认实现。
pub struct NoOpFragmenter;

impl FrameFragmenter for NoOpFragmenter {
    fn fragment(&self, data: &[u8], _mtu: usize) -> Vec<Vec<u8>> {
        vec![data.to_vec()]
    }

    fn reassemble(
        &self,
        _frame_seq: u32,
        _fragment_index: u32,
        _fragment_count: u32,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        Some(data.to_vec())
    }
}
