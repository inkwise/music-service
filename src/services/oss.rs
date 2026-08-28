//! 阿里云 OSS 存储后端（基于 reqwest + HMAC-SHA1 签名实现）。
//! 实现 StorageService trait，替代 Go 的 aliyun-oss-go-sdk。

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use async_trait::async_trait;

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::config::Config;
use crate::services::storage::StorageService;

type HmacSha1 = Hmac<Sha1>;

pub struct OSSService {
    endpoint: String,
    access_key_id: String,
    access_key_secret: String,
    bucket_name: String,
    http_client: reqwest::Client,
    config: Config,
}

impl OSSService {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        log::info!(
            "连接到OSS - Endpoint: {}, Bucket: {}",
            cfg.oss_endpoint,
            cfg.oss_bucket_name
        );

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| format!("创建HTTP客户端失败: {}", e))?;

        Ok(OSSService {
            endpoint: cfg.oss_endpoint.clone(),
            access_key_id: cfg.oss_access_key_id.clone(),
            access_key_secret: cfg.oss_access_key_secret.clone(),
            bucket_name: cfg.oss_bucket_name.clone(),
            http_client: client,
            config: cfg.clone(),
        })
    }

    /// 验证 OSS 连接（ListObjects 1条记录）
    pub async fn validate(&self) -> Result<(), String> {
        let result = self.list_files("", 1).await;
        match result {
            Ok(_) => {
                log::info!("OSS连接验证通过");
                Ok(())
            }
            Err(e) => Err(format!(
                "验证Bucket权限失败: {}\n请检查:\n1. AccessKey ID/Secret 是否正确\n2. Bucket名称是否正确\n3. Endpoint地址是否匹配",
                e
            )),
        }
    }

    /// 构建 OSS 请求 URL
    fn build_url(&self, object_key: &str) -> String {
        format!(
            "https://{}.{}/{}",
            self.bucket_name, self.endpoint, object_key
        )
    }

    /// 生成 OSS Authorization header（HMAC-SHA1 签名）
    fn sign_request(
        &self,
        method: &str,
        object_key: &str,
        headers: &BTreeMap<String, String>,
        params: &BTreeMap<String, String>,
    ) -> String {
        // CanonicalizedResource
        let resource = format!("/{}/{}", self.bucket_name, object_key);

        // Content-MD5 和 Content-Type（从 headers 中提取或默认空）
        let content_md5 = headers.get("Content-MD5").map(|s| s.as_str()).unwrap_or("");
        let content_type = headers.get("Content-Type").map(|s| s.as_str()).unwrap_or("");

        // Date header
        let date = headers
            .get("Date")
            .cloned()
            .unwrap_or_else(|| current_gmt_date());

        // CanonicalizedOSSHeaders: 所有 x-oss- 开头的 header
        let mut oss_headers: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in headers {
            let lower = k.to_lowercase();
            if lower.starts_with("x-oss-") {
                oss_headers.insert(lower, v.trim().to_string());
            }
        }
        let canonicalized_oss_headers: String = oss_headers
            .iter()
            .map(|(k, v)| format!("{}:{}", k, v))
            .collect::<Vec<_>>()
            .join("\n");
        let canonicalized_oss_headers = if canonicalized_oss_headers.is_empty() {
            String::new()
        } else {
            format!("\n{}", canonicalized_oss_headers)
        };

        // CanonicalizedResource with sub-resources
        let mut resource_with_params = resource;
        if !params.is_empty() {
            let param_str: Vec<String> = params
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.clone()
                    } else {
                        format!("{}={}", k, v)
                    }
                })
                .collect();
            resource_with_params.push_str(&format!("?{}", param_str.join("&")));
        }

        // StringToSign
        let string_to_sign = format!(
            "{}\n{}\n{}\n{}{}\n{}",
            method, content_md5, content_type, date, canonicalized_oss_headers, resource_with_params
        );

        // HMAC-SHA1 签名
        let mut mac = HmacSha1::new_from_slice(self.access_key_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(string_to_sign.as_bytes());
        let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        format!("OSS {}:{}", self.access_key_id, signature)
    }
}

fn current_gmt_date() -> String {
    use chrono::Datelike;
    use chrono::Timelike;

    let now = chrono::Utc::now();
    let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        weekdays[now.weekday().num_days_from_sunday() as usize],
        now.day(),
        months[(now.month() - 1) as usize],
        now.year(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

#[async_trait]
impl StorageService for OSSService {
    async fn upload_file(&self, object_key: &str, data: Vec<u8>) -> Result<String, String> {
        log::info!("正在上传文件到OSS: {}", object_key);
        let url = self.build_url(object_key);
        let content_type = mime_guess::from_path(object_key)
            .first_or_octet_stream()
            .to_string();

        let date = current_gmt_date();
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".to_string(), content_type);
        headers.insert("Date".to_string(), date.clone());

        let auth = self.sign_request("PUT", object_key, &headers, &BTreeMap::new());

        let resp = self
            .http_client
            .put(&url)
            .header("Authorization", &auth)
            .header("Date", &date)
            .header("Content-Type", headers.get("Content-Type").unwrap())
            .body(data)
            .send()
            .await
            .map_err(|e| format!("上传文件失败: {}", e))?;

        if resp.status().is_success() {
            log::info!("文件上传成功: {}", object_key);
            Ok(object_key.to_string())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!("上传文件失败: HTTP {} - {}", status, body))
        }
    }

    async fn delete_file(&self, object_key: &str) -> Result<(), String> {
        let url = self.build_url(object_key);
        let date = current_gmt_date();
        let mut headers = BTreeMap::new();
        headers.insert("Date".to_string(), date.clone());

        let auth = self.sign_request("DELETE", object_key, &headers, &BTreeMap::new());

        let resp = self
            .http_client
            .delete(&url)
            .header("Authorization", &auth)
            .header("Date", &date)
            .send()
            .await
            .map_err(|e| format!("删除文件失败: {}", e))?;

        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            log::info!("文件已删除: {}", object_key);
            Ok(())
        } else {
            Err(format!("删除文件失败: HTTP {}", resp.status()))
        }
    }

    async fn get_file_url(&self, object_key: &str, expire_duration: Duration) -> Result<String, String> {
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + expire_duration.as_secs();

        let url = self.build_url(object_key);

        // 签名 URL (简化版：仅签名 expires 参数)
        // 完整 OSS 签名 URL 需要包含 CanonicalizedResource 中子资源参数
        let string_to_sign = format!("GET\n\n\n{}\n/{}/{}", expires, self.bucket_name, object_key);

        let mut mac = HmacSha1::new_from_slice(self.access_key_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(string_to_sign.as_bytes());
        let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        let encoded_sig = urlencoding(&signature);
        let signed_url = format!(
            "{}?OSSAccessKeyId={}&Expires={}&Signature={}",
            url, self.access_key_id, expires, encoded_sig
        );

        log::info!("生成签名URL: {}", signed_url);
        Ok(signed_url)
    }

    async fn get_file_content(&self, object_key: &str) -> Result<Vec<u8>, String> {
        let url = self.build_url(object_key);
        let date = current_gmt_date();
        let mut headers = BTreeMap::new();
        headers.insert("Date".to_string(), date.clone());

        let auth = self.sign_request("GET", object_key, &headers, &BTreeMap::new());

        let resp = self
            .http_client
            .get(&url)
            .header("Authorization", &auth)
            .header("Date", &date)
            .send()
            .await
            .map_err(|e| format!("获取文件内容失败: {}", e))?;

        if resp.status().is_success() {
            resp.bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| format!("读取响应失败: {}", e))
        } else {
            Err(format!("获取文件内容失败: HTTP {}", resp.status()))
        }
    }

    async fn get_file_range(
        &self,
        object_key: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<u8>, String> {
        let url = self.build_url(object_key);
        let date = current_gmt_date();
        let range_value = format!("bytes={}-{}", start, end);

        let mut headers = BTreeMap::new();
        headers.insert("Date".to_string(), date.clone());
        headers.insert("Range".to_string(), range_value.clone());
        headers.insert("x-oss-range-behavior".to_string(), "standard".to_string());

        let auth = self.sign_request("GET", object_key, &headers, &BTreeMap::new());

        let resp = self
            .http_client
            .get(&url)
            .header("Authorization", &auth)
            .header("Date", &date)
            .header("Range", &range_value)
            .send()
            .await
            .map_err(|e| format!("获取文件范围失败: {}", e))?;

        if resp.status().is_success() || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            resp.bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| format!("读取响应失败: {}", e))
        } else {
            Err(format!("获取文件范围失败: HTTP {}", resp.status()))
        }
    }

    async fn list_files(&self, prefix: &str, max_keys: i32) -> Result<Vec<String>, String> {
        let url = self.build_url("");
        let date = current_gmt_date();
        let mut headers = BTreeMap::new();
        headers.insert("Date".to_string(), date.clone());

        let mut params = BTreeMap::new();
        if !prefix.is_empty() {
            params.insert("prefix".to_string(), prefix.to_string());
        }
        if max_keys > 0 {
            params.insert("max-keys".to_string(), max_keys.to_string());
        }

        let auth = self.sign_request("GET", "", &headers, &params);

        let mut req = self
            .http_client
            .get(&url)
            .header("Authorization", &auth)
            .header("Date", &date);

        for (k, v) in &params {
            req = req.query(&[(k.as_str(), v.as_str())]);
        }

        let resp = req.send().await.map_err(|e| format!("列出文件失败: {}", e))?;

        if resp.status().is_success() {
            let xml_body = resp.text().await.map_err(|e| format!("读取响应失败: {}", e))?;
            parse_list_objects_xml(&xml_body)
        } else {
            Err(format!("列出文件失败: HTTP {}", resp.status()))
        }
    }

    async fn get_local_path(
        &self,
        object_key: &str,
    ) -> Result<(String, Box<dyn FnOnce() + Send>), String> {
        let data = self.get_file_content(object_key).await?;

        // 保留原始扩展名
        let ext = std::path::Path::new(object_key)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let ext_dot = if ext.is_empty() {
            String::new()
        } else {
            format!(".{}", ext)
        };

        let tmp_dir = std::env::temp_dir();
        let tmp_name = format!("oss-dl-{}{}", uuid::Uuid::new_v4(), ext_dot);
        let tmp_path = tmp_dir.join(&tmp_name);

        std::fs::write(&tmp_path, &data).map_err(|e| format!("创建临时文件失败: {}", e))?;

        let path_clone = tmp_path.to_string_lossy().to_string();
        Ok((
            path_clone.clone(),
            Box::new(move || {
                let _ = std::fs::remove_file(&path_clone);
            }),
        ))
    }
}

/// URL 编码（仅编码特殊字符，保持 RFC 3986 兼容）
fn urlencoding(input: &str) -> String {
    let mut result = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// 简易 XML 解析：从 ListObjects 响应中提取 Key 列表
fn parse_list_objects_xml(xml: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    // 简易解析，提取 <Key>...</Key> 的内容
    let mut in_key = false;
    let mut current_key = String::new();

    for line in xml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<Key>") && trimmed.ends_with("</Key>") {
            let key = &trimmed[5..trimmed.len() - 6];
            keys.push(key.to_string());
        } else if trimmed.starts_with("<Key>") {
            in_key = true;
            current_key = trimmed[5..].to_string();
        } else if trimmed.ends_with("</Key>") && in_key {
            current_key.push_str(&trimmed[..trimmed.len() - 6]);
            keys.push(current_key.clone());
            in_key = false;
            current_key.clear();
        } else if in_key {
            current_key.push_str(trimmed);
        }
    }

    Ok(keys)
}
