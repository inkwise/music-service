//! 数据模型定义：所有 GORM 实体对应的 Rust 结构体 + 请求/响应 DTO。
//! Go 项目使用 GORM 软删除（deleted_at），Rust 中通过 deleted_at 字段 + 查询条件模拟。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================
// 歌手
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Artist {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub name: String,
    pub description: String,
    pub avatar_url: String,
}

// ============================================================
// 歌曲（核心模型）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, Default)]
pub struct Music {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,

    // 基础标签
    pub title: String,
    pub album: String,
    pub genre: String,
    pub lyrics: String,

    // 音频属性
    pub duration: f64,
    pub size: i64,
    pub bitrate: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub format: String,
    pub codec: String,
    pub channel_count: i32,

    // 排序
    pub sort_order: i32,

    // 存储 Key
    pub oss_key: String,
    pub lyrics_key: String,
    pub cover_key: String,

    // 指纹与去重
    pub fingerprint: String,
    pub md5: String,
}

/// API 响应中用于注入临时 URL 的完整 Music 结构
#[derive(Debug, Clone, Serialize)]
pub struct MusicEnriched {
    #[serde(flatten)]
    pub base: Music,
    pub artists: Vec<Artist>,
    pub download_url: String,
    pub stream_url: String,
    pub cover_url: String,
    pub lyrics_url: String,
    pub playlists: Option<Vec<Playlist>>,
}

// ============================================================
// 音乐-歌手关联表
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MusicArtist {
    pub id: u32,
    pub music_id: u32,
    pub artist_id: u32,
    pub created_at: NaiveDateTime,
}

// ============================================================
// 歌单
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Playlist {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub name: String,
    pub description: String,
    pub cover_url: String,
    pub user_id: u32,
}

// ============================================================
// 歌单-歌曲关联表
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PlaylistMusic {
    pub id: u32,
    pub playlist_id: u32,
    pub music_id: u32,
    pub added_at: NaiveDateTime,
    pub sort_order: i32,
}

// ============================================================
// 用户
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub username: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub email: String,
    pub avatar: String,
}

/// 对外暴露的用户信息（不含密码）
#[derive(Debug, Clone, Serialize)]
pub struct UserPublic {
    pub id: u32,
    pub username: String,
    pub email: String,
    pub avatar: String,
    pub avatar_url: String,
    pub created_at: NaiveDateTime,
}

// ============================================================
// 分享
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Share {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub music_id: u32,
    pub token: String,
    pub expires_at: NaiveDateTime,
}

// ============================================================
// 设备
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Device {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub user_id: u32,
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub ip_address: String,
    pub is_online: bool,
    pub role: String,
    pub sync_enabled: bool,
    pub last_seen: NaiveDateTime,
}

// ============================================================
// 同步房间 + 房间成员（保留扩展，当前项目以用户为单位同步）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SyncRoom {
    pub id: u32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub room_id: String,
    pub name: String,
    pub host_user_id: u32,
    pub host_device_id: String,
    pub status: String,
    pub current_song_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RoomMember {
    pub id: u32,
    pub joined_at: NaiveDateTime,
    pub room_id: u32,
    pub device_id: String,
    pub user_id: u32,
    pub role: String,
    pub is_connected: bool,
}

// ============================================================
// 请求/响应 DTO
// ============================================================

/// 批量指纹查询请求
#[derive(Debug, Deserialize)]
pub struct FingerprintCheckRequest {
    pub queries: Vec<FingerprintQuery>,
    pub duration_tolerance: Option<f64>,
    pub min_similarity: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct FingerprintQuery {
    pub fingerprint: String,
    pub duration: f64,
}

/// 批量指纹查询响应
#[derive(Debug, Serialize)]
pub struct FingerprintCheckResponse {
    pub results: Vec<FingerprintCheckResult>,
}

#[derive(Debug, Serialize)]
pub struct FingerprintCheckResult {
    pub query_index: usize,
    pub matched: bool,
    pub similarity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music: Option<Value>,
}

/// 通用分页响应
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T: Serialize> {
    pub data: Vec<T>,
    pub pagination: Pagination,
}

#[derive(Debug, Serialize)]
pub struct Pagination {
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
    pub total_pages: i64,
}

/// 元数据提取结果（用于上传流程）
#[derive(Debug, Clone)]
pub struct MusicMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub track: i32,
    pub year: i32,
    pub lyrics: String,
    pub format: String,
    pub codec: String,
    pub duration: f64,
    pub size: i64,
    pub bitrate: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub cover_data: Vec<u8>,
    pub cover_mime: String,
}
