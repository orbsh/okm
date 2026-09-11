//! 共享测试类型（跨测试文件复用）
#![allow(dead_code)]
#![cfg(any(feature = "fjall", feature = "slatedb"))]

use okm_core::{EdgeEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(1)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(2)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}

#[derive(EdgeEncode, Clone)]
#[kv_ns(4)]
pub struct UserToSessionEdge {
    #[kv_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}
