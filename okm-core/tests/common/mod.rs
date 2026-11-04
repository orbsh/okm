//! 共享测试类型（跨测试文件复用）
#![allow(dead_code)]
#![cfg(any(feature = "fjall", feature = "slatedb"))]

use okm_core::{DocumentEncode, KeyEncode};

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

/// 关系表（ADR-0051）：键是两个身份，行上带关系自有的列，反向方向是声明索引。
/// 它取代了原来的 junction 声明——一张普通的集合。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct UserSessionKey {
    pub user_id: u64,
    pub session_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserSessionKey)]
#[ok_ns(112)]
#[ok_index(by_session { fields(session_id) })]
pub struct UserSessionRow {
    pub session_id: u64,
}
