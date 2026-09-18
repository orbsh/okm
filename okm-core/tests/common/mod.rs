//! 共享测试类型（跨测试文件复用）
#![allow(dead_code)]
#![cfg(any(feature = "fjall", feature = "slatedb"))]

use okm_core::{DocumentEncode, JunctionEncode, KeyEncode, Ref};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}

/// 端点文档：user（ns=1）
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(1)]
pub struct User {
    pub org_id: u32,
    pub user_id: u64,
}

/// 端点文档：session（ns=2）
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(SessionKey)]
#[ok_ns(2)]
pub struct Session {
    pub org_id: u32,
    pub session_id: u64,
}

#[derive(JunctionEncode, Clone)]
#[ok_junction(1)]
pub struct UserToSessionEdge {
    #[ok_head(org_id, user_id)]
    pub user_id: Ref<User, UserKey>,
    pub session_id: Ref<Session, SessionKey>,
}
