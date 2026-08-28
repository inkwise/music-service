//! 本地文件系统存储后端，实现 StorageService trait。

use std::path::{Path, PathBuf};
use std::time::Duration;
use async_trait::async_trait;

use crate::services::storage::StorageService;

pub struct LocalStorageService {
    base_path: PathBuf,
}

impl LocalStorageService {
    pub fn new(base_path: &str) -> Result<Self, String> {
        let abs_path = std::path::absolute(Path::new(base_path))
            .map_err(|e| format!("解析存储路径失败: {}", e))?;
        std::fs::create_dir_all(&abs_path)
            .map_err(|e| format!("创建存储目录失败: {}", e))?;
        log::info!("本地存储初始化成功，目录: {}", abs_path.display());
        Ok(LocalStorageService { base_path: abs_path })
    }

    fn full_path(&self, object_key: &str) -> PathBuf {
        self.base_path.join(object_key)
    }
}

#[async_trait]
impl StorageService for LocalStorageService {
    async fn upload_file(&self, object_key: &str, data: Vec<u8>) -> Result<String, String> {
        let full_path = self.full_path(object_key);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
        }
        std::fs::write(&full_path, &data).map_err(|e| format!("写入文件失败: {}", e))?;
        log::info!("文件已保存到本地: {}", full_path.display());
        Ok(object_key.to_string())
    }

    async fn delete_file(&self, object_key: &str) -> Result<(), String> {
        let full_path = self.full_path(object_key);
        match std::fs::remove_file(&full_path) {
            Ok(()) => {
                log::info!("本地文件已删除: {}", full_path.display());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("删除文件失败: {}", e)),
        }
    }

    async fn get_file_url(&self, _object_key: &str, _expire: Duration) -> Result<String, String> {
        Err("本地存储不支持直接URL访问，请使用流媒体或代理下载接口".into())
    }

    async fn get_file_content(&self, object_key: &str) -> Result<Vec<u8>, String> {
        let full_path = self.full_path(object_key);
        std::fs::read(&full_path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => format!("文件不存在: {}", object_key),
                _ => format!("读取文件失败: {}", e),
            })
    }

    async fn get_file_range(&self, object_key: &str, start: i64, end: i64) -> Result<Vec<u8>, String> {
        // 先做范围校验，避免 start > end 切片 panic
        if start < 0 || end < start {
            return Err(format!("无效的字节范围: {}-{}", start, end));
        }
        let full_path = self.full_path(object_key);
        let data = std::fs::read(&full_path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => format!("文件不存在: {}", object_key),
                _ => format!("读取文件失败: {}", e),
            })?;

        let len = data.len();
        if start >= len as i64 {
            return Err(format!("起始位置 {} 超出文件长度 {}", start, len));
        }
        let s = start as usize;
        let e = (end as usize).min(len - 1);
        Ok(data[s..=e].to_vec())
    }

    async fn list_files(&self, prefix: &str, max_keys: i32) -> Result<Vec<String>, String> {
        let mut files = Vec::new();
        self.walk_dir(&self.base_path, prefix, &mut files, max_keys)?;
        Ok(files)
    }

    async fn get_local_path(
        &self,
        object_key: &str,
    ) -> Result<(String, Box<dyn FnOnce() + Send>), String> {
        let full_path = self.full_path(object_key);
        if !full_path.exists() {
            return Err(format!("文件不存在: {}", object_key));
        }
        Ok((full_path.to_string_lossy().to_string(), Box::new(|| {})))
    }
}

impl LocalStorageService {
    fn walk_dir(&self, dir: &Path, prefix: &str, files: &mut Vec<String>, max_keys: i32) -> Result<(), String> {
        if max_keys > 0 && files.len() >= max_keys as usize {
            return Ok(());
        }

        let entries = std::fs::read_dir(dir).map_err(|e| format!("遍历目录失败: {}", e))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                self.walk_dir(&path, prefix, files, max_keys)?;
            } else if path.is_file() {
                let rel = path.strip_prefix(&self.base_path).unwrap_or(&path);
                let rel_str = rel.to_string_lossy();
                if prefix.is_empty() || rel_str.starts_with(prefix) {
                    files.push(rel_str.to_string());
                    if max_keys > 0 && files.len() >= max_keys as usize {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}
