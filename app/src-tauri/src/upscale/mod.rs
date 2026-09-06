//! waifu2x 原生移植（P1：CPU 推理）
//!
//! 结构：
//! - caffemodel.rs：Caffe protobuf 权重解析
//! - model.rs：网络拓扑（层执行序列、形状推算）
//! - cpu.rs：conv/deconv/leaky ReLU CPU 推理引擎（行级并行）
//! - pipeline.rs：分块/缩放编排（对应原版 ReconstructImage）
//! - commands.rs：Tauri 命令

pub mod caffemodel;
pub mod commands;
pub mod cpu;
pub mod d3d11;
pub mod itm;
pub mod model;
pub mod pipeline;
pub mod pth;

pub use model::{ModelKind, WaifuModel};
