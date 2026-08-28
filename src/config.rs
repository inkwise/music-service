//! 应用配置：从 config.toml 文件加载，支持 SQLite/MySQL 双数据库和 OSS/本地双存储后端。
//!
//! 配置结构体使用 serde 反序列化 TOML，层次化分组：
//!   [server], [database], [storage], [storage.oss], [jwt], [fingerprint], [share]
//!
//! 首次启动时如果 config.toml 不存在，会自动生成带注释的模板文件。

use serde::{Deserialize, Serialize};

/// 顶层配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub server: ServerSection,
    pub database: DatabaseSection,
    pub storage: StorageSection,
    pub jwt: JwtSection,
    pub fingerprint: FingerprintSection,
    pub share: ShareSection,
}

/// [server]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_port")]
    pub port: u16,
}

/// [database]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSection {
    #[serde(default = "default_db_type")]
    pub db_type: String,           // "sqlite" 或 "mysql"

    #[serde(default = "default_db_path")]
    pub path: String,              // SQLite 文件路径

    #[serde(default = "default_host")]
    pub host: String,              // MySQL 主机地址

    #[serde(default = "default_db_port")]
    pub port: u16,                 // MySQL 端口

    #[serde(default = "default_user")]
    pub user: String,              // MySQL 用户名

    #[serde(default)]
    pub password: String,          // MySQL 密码

    #[serde(default = "default_db_name")]
    pub name: String,              // MySQL 数据库名
}

/// [storage]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_storage_type")]
    pub storage_type: String,      // "oss" 或 "local"

    #[serde(default = "default_local_path")]
    pub local_path: String,        // 本地存储目录

    #[serde(default)]
    pub oss: OssSection,           // OSS 子配置
}

/// [storage.oss]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OssSection {
    #[serde(default = "default_oss_endpoint")]
    pub endpoint: String,

    #[serde(default)]
    pub access_key_id: String,

    #[serde(default)]
    pub access_key_secret: String,

    #[serde(default = "default_oss_bucket")]
    pub bucket_name: String,
}

/// [jwt]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtSection {
    #[serde(default = "default_jwt_secret")]
    pub secret: String,
}

/// [fingerprint]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintSection {
    #[serde(default = "default_fp_workers")]
    pub max_workers: usize,

    #[serde(default = "default_fp_min_similarity")]
    pub min_similarity: f64,

    #[serde(default = "default_fp_duration_tolerance")]
    pub duration_tolerance: f64,
}

/// [share]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSection {
    #[serde(default = "default_share_expiry")]
    pub expiry_hours: i64,
}

// ---- 默认值函数 ----
fn default_port() -> u16            { 8080 }
fn default_db_type() -> String      { "sqlite".into() }
fn default_db_path() -> String      { "./data/music.db".into() }
fn default_host() -> String         { "127.0.0.1".into() }
fn default_db_port() -> u16         { 3306 }
fn default_user() -> String         { "root".into() }
fn default_db_name() -> String      { "music_service".into() }
fn default_storage_type() -> String { "oss".into() }
fn default_local_path() -> String   { "./uploads".into() }
fn default_oss_endpoint() -> String { "oss-cn-hangzhou.aliyuncs.com".into() }
fn default_oss_bucket() -> String   { "your-bucket-name".into() }
fn default_jwt_secret() -> String   { "my-super-secret-key-12345".into() }
fn default_fp_workers() -> usize    { 2 }
fn default_fp_min_similarity() -> f64 { 0.85 }
fn default_fp_duration_tolerance() -> f64 { 10.0 }
fn default_share_expiry() -> i64    { 4 }

/// 扁平化的运行时配置（供 handler 使用，保持与原来相同的字段名）
#[derive(Debug, Clone)]
pub struct Config {
    pub server_port: String,
    pub db_type: String,
    pub db_path: String,
    pub db_host: String,
    pub db_port: String,
    pub db_user: String,
    pub db_password: String,
    pub db_name: String,
    pub storage_type: String,
    pub local_storage_path: String,
    pub oss_endpoint: String,
    pub oss_access_key_id: String,
    pub oss_access_key_secret: String,
    pub oss_bucket_name: String,
    pub jwt_secret: String,
    pub fingerprint_max_workers: usize,
    pub fingerprint_min_similarity: f64,
    pub fingerprint_duration_tolerance: f64,
    pub share_expiry_hours: i64,
}

const CONFIG_PATH: &str = "config.toml";

impl Config {
    /// 加载配置：优先读取 config.toml，不存在则创建模板。
    pub fn load() -> Self {
        let raw = match std::fs::metadata(CONFIG_PATH) {
            Err(_) => {
                let tmpl = default_template();
                let _ = std::fs::write(CONFIG_PATH, &tmpl);
                eprintln!("已创建 config.toml 模板文件，请根据需要修改配置");
                tmpl
            }
            Ok(_) => std::fs::read_to_string(CONFIG_PATH).unwrap_or_else(|_| default_template()),
        };

        let cf: ConfigFile = toml::from_str(&raw).unwrap_or_else(|e| {
            eprintln!("解析 config.toml 失败: {}，使用默认配置", e);
            toml::from_str(&default_template()).unwrap()
        });

        Config {
            server_port: cf.server.port.to_string(),
            db_type: cf.database.db_type,
            db_path: cf.database.path,
            db_host: cf.database.host,
            db_port: cf.database.port.to_string(),
            db_user: cf.database.user,
            db_password: cf.database.password,
            db_name: cf.database.name,
            storage_type: cf.storage.storage_type,
            local_storage_path: cf.storage.local_path,
            oss_endpoint: cf.storage.oss.endpoint,
            oss_access_key_id: cf.storage.oss.access_key_id,
            oss_access_key_secret: cf.storage.oss.access_key_secret,
            oss_bucket_name: cf.storage.oss.bucket_name,
            jwt_secret: cf.jwt.secret,
            fingerprint_max_workers: cf.fingerprint.max_workers,
            fingerprint_min_similarity: cf.fingerprint.min_similarity,
            fingerprint_duration_tolerance: cf.fingerprint.duration_tolerance,
            share_expiry_hours: cf.share.expiry_hours,
        }
    }

    /// 校验必填配置项
    pub fn validate(&self) -> Result<(), String> {
        if self.storage_type == "oss" {
            if self.oss_access_key_id.is_empty() {
                return Err("storage.oss.access_key_id is required".into());
            }
            if self.oss_access_key_secret.is_empty() {
                return Err("storage.oss.access_key_secret is required".into());
            }
            if self.oss_bucket_name.is_empty() || self.oss_bucket_name == "your-bucket-name" {
                return Err("storage.oss.bucket_name is not properly configured".into());
            }
            if self.oss_endpoint.is_empty() {
                return Err("storage.oss.endpoint is required".into());
            }
        }
        Ok(())
    }

    /// 根据 DBType 返回数据库连接 URL（用于 sqlx）
    pub fn database_url(&self) -> String {
        if self.db_type == "mysql" {
            format!(
                "mysql://{}:{}@{}:{}/{}?charset=utf8mb4",
                self.db_user, self.db_password, self.db_host, self.db_port, self.db_name
            )
        } else {
            // canonicalize 确保路径存在父目录；SQLite 会自动创建 db 文件
            let path = std::path::Path::new(&self.db_path);
            if let Some(parent) = path.parent() {
                if parent != std::path::Path::new("") {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
            format!("sqlite://{}?mode=rwc", abs.display())
        }
    }

    /// MySQL DSN（不含数据库名，用于创建数据库）
    pub fn mysql_dsn_no_db(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/",
            self.db_user, self.db_password, self.db_host, self.db_port
        )
    }

    /// 将当前配置序列化写入 config.toml
    pub fn save_to_file(&self) -> std::io::Result<()> {
        let cf = ConfigFile {
            server: ServerSection {
                port: self.server_port.parse().unwrap_or(8080),
            },
            database: DatabaseSection {
                db_type: self.db_type.clone(),
                path: self.db_path.clone(),
                host: self.db_host.clone(),
                port: self.db_port.parse().unwrap_or(3306),
                user: self.db_user.clone(),
                password: self.db_password.clone(),
                name: self.db_name.clone(),
            },
            storage: StorageSection {
                storage_type: self.storage_type.clone(),
                local_path: self.local_storage_path.clone(),
                oss: OssSection {
                    endpoint: self.oss_endpoint.clone(),
                    access_key_id: self.oss_access_key_id.clone(),
                    access_key_secret: self.oss_access_key_secret.clone(),
                    bucket_name: self.oss_bucket_name.clone(),
                },
            },
            jwt: JwtSection {
                secret: self.jwt_secret.clone(),
            },
            fingerprint: FingerprintSection {
                max_workers: self.fingerprint_max_workers,
                min_similarity: self.fingerprint_min_similarity,
                duration_tolerance: self.fingerprint_duration_tolerance,
            },
            share: ShareSection {
                expiry_hours: self.share_expiry_hours,
            },
        };

        let toml_str = toml::to_string_pretty(&cf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // 在文件头部写入注释说明
        let header = r#"# Music Service 配置文件
# 修改后重启服务即可生效
#
# 数据库类型: "sqlite" 或 "mysql"
# 存储类型:   "oss" 或 "local"
# 详细配置见各节注释

"#;

        std::fs::write(CONFIG_PATH, format!("{}{}", header, toml_str))
    }
}

/// 生成带注释的默认配置模板
fn default_template() -> String {
    r#"# Music Service 配置文件
# 修改后重启服务即可生效

[server]
# HTTP 监听端口
port = 8080

[database]
# 数据库类型: "sqlite" 或 "mysql"
db_type = "sqlite"
# SQLite 模式
path = "./data/music.db"
# MySQL 模式 (仅 db_type = "mysql" 时生效)
host = "127.0.0.1"
port = 3306
user = "root"
password = ""
name = "music_service"

[storage]
# 存储类型: "oss" 或 "local"
storage_type = "oss"
# 本地存储模式 (仅 storage_type = "local" 时生效)
local_path = "./uploads"

[storage.oss]
# 阿里云 OSS 配置 (仅 storage_type = "oss" 时生效)
endpoint = "oss-cn-hangzhou.aliyuncs.com"
access_key_id = ""
access_key_secret = ""
bucket_name = "your-bucket-name"

[jwt]
# JWT 签名密钥 (请务必修改为随机字符串)
secret = "my-super-secret-key-12345"

[fingerprint]
# fpcalc 最大并发数
max_workers = 2
# 指纹匹配最低相似度 (0-1)
min_similarity = 0.85
# 时长容差 (秒)
duration_tolerance = 10.0

[share]
# 分享链接有效期 (小时)
expiry_hours = 4
"#.to_string()
}
