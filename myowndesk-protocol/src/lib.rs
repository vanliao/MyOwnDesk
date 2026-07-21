pub mod crypto;
pub mod fragment;

// prost 从 messages.proto 编译生成的 Rust 代码
include!(concat!(env!("OUT_DIR"), "/myowndesk.rs"));

pub use crypto::{FrameCipher, NoOpCipher};
pub use fragment::{FrameFragmenter, NoOpFragmenter};
