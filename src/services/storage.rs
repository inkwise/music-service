//! 存储服务抽象 trait，对应 Go 的 StorageService interface。

use async_trait::async_trait;

/// 存储服务抽象接口
#[async_trait]
pub trait StorageService: Send + Sync {
    /// 上传文件，返回存储 Key
    async fn upload_file(&self, object_key: &str, data: Vec<u8>) -> Result<String, String>;

    /// 删除文件
    async fn delete_file(&self, object_key: &str) -> Result<(), String>;

    /// 获取签名 URL
    async fn get_file_url(&self, object_key: &str, expire_duration: std::time::Duration) -> Result<String, String>;

    /// 获取文件完整内容
    async fn get_file_content(&self, object_key: &str) -> Result<Vec<u8>, String>;

    /// 获取文件的指定字节范围 [start, end]
    async fn get_file_range(&self, object_key: &str, start: i64, end: i64) -> Result<Vec<u8>, String>;

    /// 列出以 prefix 开头的文件
    async fn list_files(&self, prefix: &str, max_keys: i32) -> Result<Vec<String>, String>;

    /// 获取文件的本地路径，返回 (path, cleanup_fn)
    async fn get_local_path(&self, object_key: &str) -> Result<(String, Box<dyn FnOnce() + Send>), String>;
}

// ============================================================
// Key 生成工具函数
// ============================================================

pub fn generate_object_key(filename: &str) -> String {
    let filename = sanitize_filename(filename);
    let timestamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("music/{}_{}", timestamp, filename)
}

pub fn generate_cover_key(music_key: &str, ext: &str) -> String {
    let base = match music_key.rfind('.') {
        Some(i) => &music_key[..i],
        None => music_key,
    };
    format!("{}_cover{}", base, ext)
}

pub fn generate_lyrics_key(music_key: &str) -> String {
    let base = match music_key.rfind('.') {
        Some(i) => &music_key[..i],
        None => music_key,
    };
    format!("{}_lyrics.lrc", base)
}

fn sanitize_filename(filename: &str) -> String {
    let (name, ext) = match filename.rfind('.') {
        Some(i) => (&filename[..i], &filename[i..]),
        None => (filename, ""),
    };

    let cleaned: String = name
        .chars()
        .map(|c| match c {
            ' ' | '\u{3000}' => '_',
            '（' | '）' | '(' | ')' | '【' | '】' | '[' | ']' => '_',
            '：' | ':' | '、' | '，' | ',' => '_',
            '#' | '&' | '!' | '@' | '$' | '%' | '^' | '*' | '+' | '=' | '|' => '_',
            '\\' | '/' | '<' | '>' | '?' | '`' | '\'' | '"' | '~' | ';' => '_',
            other => other,
        })
        .collect();

    let mut result = cleaned;
    while result.contains("__") {
        result = result.replace("__", "_");
    }

    let trimmed = result.trim_matches(|c: char| c == '_' || c == '-' || c == '.');
    if trimmed.is_empty() {
        format!("audio_{}{}", chrono::Utc::now().timestamp(), ext)
    } else {
        format!("{}{}", trimmed, ext)
    }
}
