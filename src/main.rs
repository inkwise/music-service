mod config;
mod handlers;
mod middleware;
mod models;
mod services;

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, HeaderName, StatusCode},
    middleware as axum_middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use md5::{Digest, Md5};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePoolOptions;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use config::Config;
use models::*;
use services::{
    fingerprint::{self, FingerprintService},
    local_storage::LocalStorageService,
    metadata::MetadataExtractor,
    oss::OSSService,
    storage,
    ws_hub, Hub, StorageService,
};

/// 全局共享状态
struct AppState {
    db: sqlx::SqlitePool,
    storage: Arc<dyn StorageService>,
    fingerprint: Arc<FingerprintService>,
    config: Config,
    metadata: Arc<MetadataExtractor>,
    hub: Arc<Hub>,
    initialized: AtomicBool,
    start_time: std::time::Instant,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = Config::load();
    let init_marker = ".initialized";
    let initialized = std::path::Path::new(init_marker).exists();

    if !initialized {
        log::info!("系统未初始化，进入设置模式");

        let state = Arc::new(AppState {
            db: open_db_simple(&cfg).await,
            storage: Arc::new(LocalStorageService::new("./uploads").unwrap()),
            fingerprint: Arc::new(FingerprintService::new(1)),
            config: cfg,
            metadata: Arc::new(MetadataExtractor::new()),
            hub: Hub::new(open_db_simple(&Config::load()).await),
            initialized: AtomicBool::new(false),
            start_time: std::time::Instant::now(),
        });

        let app = Router::new()
            .route("/health", get(health_check))
            .route("/api/v1/health", get(health_check))
            .route("/api/v1/setup/status", get(setup_status))
            .route("/api/v1/setup/initialize", post(setup_initialize))
            .fallback(fallback_setup)
            .layer(axum_middleware::from_fn(middleware::cors_middleware))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", Config::load().server_port);
        log::info!("服务器启动在 {}", addr);
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
        return;
    }

    log::info!("系统已初始化，正常启动");

    // 初始化数据库
    let db = init_database(&cfg).await.expect("数据库初始化失败");
    run_migrations(&db).await.expect("数据库迁移失败");

    // 初始化存储
    let storage: Arc<dyn StorageService> = if cfg.storage_type == "local" {
        Arc::new(LocalStorageService::new(&cfg.local_storage_path).unwrap())
    } else {
        let oss = OSSService::new(&cfg).unwrap();
        if let Err(e) = oss.validate().await {
            log::error!("OSS 验证失败: {}", e);
        }
        Arc::new(oss)
    };

    // 指纹服务（当前指纹由客户端本地生成，fpcalc 仅用于预留的服务端指纹能力）
    let fp_service = Arc::new(FingerprintService::new(cfg.fingerprint_max_workers));
    if !fp_service.available() {
        log::info!("fpcalc 未安装（服务端指纹生成未启用，不影响客户端指纹查重）");
    }

    // 本地存储模式下清理孤儿文件（磁盘上存在但未被任何 DB 记录引用）
    if cfg.storage_type == "local" {
        cleanup_orphan_files(&db, &cfg.local_storage_path).await;
    }

    if !std::path::Path::new(init_marker).exists() {
        let _ = std::fs::write(init_marker, "1");
    }

    let hub = Hub::new(db.clone());

    let state = Arc::new(AppState {
        db,
        storage,
        fingerprint: fp_service,
        config: cfg,
        metadata: Arc::new(MetadataExtractor::new()),
        hub,
        initialized: AtomicBool::new(true),
        start_time: std::time::Instant::now(),
    });

    let app = Router::new()
        // 健康检查
        .route("/health", get(health_check))
        .route("/api/v1/health", get(health_check))
        // 设置状态（查询用）
        .route("/api/v1/setup/status", get(setup_status))
        // 无需认证的公共路由
        .route("/api/v1/auth/register", post(auth_register))
        .route("/api/v1/auth/login", post(auth_login))
        .route("/api/v1/ntp/time", get(ntp_time))
        .route("/api/v1/music/{id}/stream", get(stream_music))
        .route("/api/v1/music/{id}/cover", get(serve_cover))
        .route("/api/v1/music/{id}/lyrics", get(serve_lyrics))
        .route("/api/v1/music/fingerprints", get(list_fingerprints))
        .route("/api/v1/shared/{token}", get(share_page))
        .route("/api/v1/shared/{token}/stream", get(share_stream))
        .route("/api/v1/shared/{token}/cover", get(share_cover))
        // 需认证的 API
        .route("/api/v1/profile", get(auth_profile))
        .route("/api/v1/profile/avatar", post(auth_upload_avatar).get(auth_serve_avatar))
        .route("/api/v1/music/upload", post(music_upload))
        .route("/api/v1/music/upload/batch", post(music_upload_batch))
        .route("/api/v1/music/list", get(music_list))
        .route("/api/v1/music/fingerprint/check", post(music_fingerprint_check))
        .route("/api/v1/music/reorder", axum::routing::put(music_reorder))
        .route("/api/v1/music/search/suggestions", get(music_suggestions))
        .route("/api/v1/music/{id}", get(music_detail).delete(music_delete).put(music_update))
        .route("/api/v1/music/{id}/proxy-download", get(music_proxy_download))
        .route("/api/v1/music/{id}/share", post(music_share))
        .route("/api/v1/music/{id}/cover", axum::routing::put(music_update_cover))
        .route("/api/v1/music/{id}/lyrics", axum::routing::put(music_update_lyrics))
        .route("/api/v1/artists", get(artists_list))
        .route("/api/v1/artists/{id}", get(artist_detail))
        .route("/api/v1/artists/by-name/{name}", get(artist_by_name))
        .route("/api/v1/albums", get(albums_list))
        .route("/api/v1/albums/{name}/music", get(album_music))
        .route("/api/v1/playlists", post(playlist_create).get(playlists_list))
        .route("/api/v1/playlists/music/batch", axum::routing::delete(batch_delete_music))
        .route("/api/v1/playlists/{id}", get(playlist_detail).put(playlist_update).delete(playlist_delete))
        .route("/api/v1/playlists/{id}/music", post(playlist_add_music).get(playlist_get_music))
        .route("/api/v1/playlists/{id}/music/reorder", axum::routing::put(playlist_reorder_music))
        .route("/api/v1/playlists/{id}/music/{mid}", axum::routing::delete(playlist_remove_music))
        .route("/api/v1/devices/register", post(device_register))
        .route("/api/v1/devices", get(devices_list))
        .route("/api/v1/devices/{did}", axum::routing::delete(device_unregister))
        .route("/api/v1/sync/status", get(sync_status))
        .route("/api/v1/sync/toggle-slave", post(sync_toggle_slave))
        // WebSocket
        .route("/api/v1/ws", get(ws_upgrade))
        // 静态文件 & 404
        .route("/admin", get(serve_admin))
        .fallback(fallback_404)
        .layer(DefaultBodyLimit::max(500 * 1024 * 1024)) // 500MB for audio uploads
        .layer(axum_middleware::from_fn(middleware::cors_middleware))
        .with_state(state);

    let cfg2 = Config::load();
    let addr = format!("0.0.0.0:{}", cfg2.server_port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    log::info!("服务器启动在 {}", addr);
    axum::serve(listener, app).await.unwrap();
}

// ============================================================
// Handler 函数
// ============================================================

type AppStateArc = Arc<AppState>;

async fn health_check(State(s): State<AppStateArc>) -> Json<Value> {
    let mut resp = json!({
        "status": "ok",
        "initialized": s.initialized.load(std::sync::atomic::Ordering::SeqCst),
        "uptime": format!("{:?}", s.start_time.elapsed()),
    });
    if s.initialized.load(std::sync::atomic::Ordering::SeqCst) {
        resp["database"] = json!({"status": "ok"});
        resp["storage"] = json!({"status": "ok"});
        resp["fingerprint"] = json!({"available": s.fingerprint.available()});
    }
    Json(resp)
}

// ---- 设置 ----

async fn setup_status(State(s): State<AppStateArc>) -> Json<Value> {
    Json(json!({
        "initialized": s.initialized.load(std::sync::atomic::Ordering::SeqCst),
        "db_type": s.config.db_type,
        "storage_type": s.config.storage_type,
        "oss_endpoint": s.config.oss_endpoint,
        "oss_access_key_id": s.config.oss_access_key_id,
        "oss_bucket_name": s.config.oss_bucket_name,
        "local_storage_path": s.config.local_storage_path,
        "db_path": s.config.db_path,
        "db_host": s.config.db_host,
        "db_port": s.config.db_port,
        "db_user": s.config.db_user,
        "db_name": s.config.db_name,
    }))
}

async fn setup_initialize(State(s): State<AppStateArc>, Json(body): Json<Value>) -> impl IntoResponse {
    // 以当前配置为基底，按提交内容覆盖
    let mut cfg = s.config.clone();
    if let Some(v) = body["db_type"].as_str() { if v == "sqlite" || v == "mysql" { cfg.db_type = v.into(); } }
    if let Some(v) = body["db_path"].as_str() { if !v.is_empty() { cfg.db_path = v.into(); } }
    if let Some(v) = body["db_host"].as_str() { if !v.is_empty() { cfg.db_host = v.into(); } }
    if let Some(v) = body["db_port"].as_str() { if !v.is_empty() { cfg.db_port = v.into(); } }
    if let Some(p) = body["db_port"].as_u64() { cfg.db_port = p.to_string(); }
    if let Some(v) = body["db_user"].as_str() { if !v.is_empty() { cfg.db_user = v.into(); } }
    if let Some(v) = body["db_password"].as_str() { cfg.db_password = v.into(); }
    if let Some(v) = body["db_name"].as_str() { if !v.is_empty() { cfg.db_name = v.into(); } }
    if let Some(v) = body["storage_type"].as_str() { if v == "local" || v == "oss" { cfg.storage_type = v.into(); } }
    if let Some(v) = body["local_storage_path"].as_str() { if !v.is_empty() { cfg.local_storage_path = v.into(); } }
    if let Some(v) = body["oss_endpoint"].as_str() { if !v.is_empty() { cfg.oss_endpoint = v.into(); } }
    if let Some(v) = body["oss_access_key_id"].as_str() { cfg.oss_access_key_id = v.into(); }
    if let Some(v) = body["oss_access_key_secret"].as_str() { cfg.oss_access_key_secret = v.into(); }
    if let Some(v) = body["oss_bucket_name"].as_str() { if !v.is_empty() { cfg.oss_bucket_name = v.into(); } }

    // 校验（主要是 OSS 必填项）
    if let Err(e) = cfg.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }

    // 持久化 config.toml
    if let Err(e) = cfg.save_to_file() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("写入配置文件失败: {}", e)}))).into_response();
    }

    // 写初始化完成标记：服务重启后进入正常模式
    if let Err(e) = std::fs::write(".initialized", "1") {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("写入初始化标记失败: {}", e)}))).into_response();
    }

    (StatusCode::OK, Json(json!({
        "message": "初始化成功，配置已保存。请重启服务使配置生效",
        "initialized": true,
        "restart_required": true,
    }))).into_response()
}

async fn ntp_time() -> Json<Value> {
    let now = chrono::Utc::now();
    Json(json!({"server_time_ms": now.timestamp_millis(), "server_time_ns": now.timestamp_nanos_opt().unwrap_or(0)}))
}

// ---- 认证 ----

async fn auth_register(State(s): State<AppStateArc>, Json(body): Json<Value>) -> impl IntoResponse {
    let username = body["username"].as_str().unwrap_or("").to_string();
    let password = body["password"].as_str().unwrap_or("").to_string();
    let email = body["email"].as_str().unwrap_or("").to_string();
    if username.is_empty() || password.len() < 6 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "输入数据无效"}))).into_response();
    }
    let existing = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ? AND deleted_at IS NULL")
        .bind(&username).fetch_optional(&s.db).await.ok().flatten();
    if existing.is_some() {
        return (StatusCode::CONFLICT, Json(json!({"error": "用户名已存在"}))).into_response();
    }
    let hashed = bcrypt::hash(&password, bcrypt::DEFAULT_COST).unwrap();
    let now = chrono::Utc::now().naive_utc();
    let result = sqlx::query("INSERT INTO users (created_at, updated_at, username, password, email, avatar) VALUES (?,?,?,?,?,'')")
        .bind(now).bind(now).bind(&username).bind(&hashed).bind(&email)
        .execute(&s.db).await.unwrap();
    let uid = result.last_insert_rowid() as u32;
    let token = middleware::generate_jwt(uid, &s.config.jwt_secret).unwrap();
    (StatusCode::OK, Json(json!({"message":"注册成功","token":token,"user":{"id":uid,"username":username,"email":email}}))).into_response()
}

async fn auth_login(State(s): State<AppStateArc>, Json(body): Json<Value>) -> impl IntoResponse {
    let username = body["username"].as_str().unwrap_or("");
    let password = body["password"].as_str().unwrap_or("");
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ? AND deleted_at IS NULL")
        .bind(username).fetch_optional(&s.db).await.ok().flatten();
    let Some(user) = user else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"用户名或密码错误"}))).into_response();
    };
    if !bcrypt::verify(password, &user.password).unwrap_or(false) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"用户名或密码错误"}))).into_response();
    }
    let token = middleware::generate_jwt(user.id, &s.config.jwt_secret).unwrap();
    (StatusCode::OK, Json(json!({"message":"登录成功","token":token,"user":{"id":user.id,"username":user.username,"email":user.email}}))).into_response()
}

async fn auth_profile(State(s): State<AppStateArc>, headers: HeaderMap) -> impl IntoResponse {
    let uid = auth_user_id(&headers, &s.config.jwt_secret).unwrap_or(0);
    if uid == 0 { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); }
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id=? AND deleted_at IS NULL")
        .bind(uid).fetch_optional(&s.db).await.ok().flatten();
    let Some(user) = user else { return (StatusCode::NOT_FOUND, Json(json!({"error":"用户不存在"}))).into_response(); };
    let avatar_url = if user.avatar.is_empty() { String::new() } else { "/api/v1/profile/avatar".into() };
    Json(json!({"user":{"id":user.id,"username":user.username,"email":user.email,"avatar":user.avatar,"avatar_url":avatar_url}})).into_response()
}

// ============================================================
// 头像
// ============================================================

async fn auth_upload_avatar(
    State(s): State<AppStateArc>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id) => id, None => return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    let allowed = ["jpg","jpeg","png","gif","webp"];

    let mut uploaded = None;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.file_name().unwrap_or("avatar.jpg").to_string();
        let ext = std::path::Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_lowercase();
        if !allowed.contains(&ext.as_str()) {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("不支持的图片格式: {}", ext)}))).into_response();
        }
        let data = field.bytes().await.unwrap_or_default().to_vec();
        if data.is_empty() { continue; }

        let key = format!("avatars/{}_{}.{}", uid, chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0), ext);

        // 删旧头像
        let old: Option<(String,)> = sqlx::query_as("SELECT avatar FROM users WHERE id=? AND deleted_at IS NULL")
            .bind(uid).fetch_optional(&s.db).await.unwrap_or(None);
        if let Some((old_av,)) = old { if !old_av.is_empty() { let _ = s.storage.delete_file(&old_av).await; } }

        if let Err(e) = s.storage.upload_file(&key, data).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("头像存储失败: {}",e)}))).into_response();
        }
        let now = chrono::Utc::now().naive_utc();
        sqlx::query("UPDATE users SET avatar=?, updated_at=? WHERE id=?").bind(&key).bind(now).bind(uid).execute(&s.db).await.unwrap();
        uploaded = Some(key);
        break;
    }
    match uploaded {
        Some(k) => Json(json!({"message":"头像上传成功","avatar":k,"avatar_url":"/api/v1/profile/avatar"})).into_response(),
        None => (StatusCode::BAD_REQUEST, Json(json!({"error":"没有上传头像文件"}))).into_response(),
    }
}

async fn auth_serve_avatar(State(s): State<AppStateArc>, headers: HeaderMap) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id) => id, None => return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    let row: Option<(String,)> = sqlx::query_as("SELECT avatar FROM users WHERE id=? AND deleted_at IS NULL")
        .bind(uid).fetch_optional(&s.db).await.unwrap_or(None);
    let Some((key,)) = row else { return (StatusCode::NOT_FOUND, Json(json!({"error":"头像不存在"}))).into_response() };
    if key.is_empty() { return (StatusCode::NOT_FOUND, Json(json!({"error":"头像不存在"}))).into_response(); }

    match s.storage.get_file_content(&key).await {
        Ok(data) => {
            let ext = std::path::Path::new(&key).extension().and_then(|e| e.to_str()).unwrap_or("jpg");
            let ct = match ext { "png"=>"image/png","gif"=>"image/gif","webp"=>"image/webp", _=>"image/jpeg" };
            (StatusCode::OK, [(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "public, max-age=3600")], data).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error":"头像文件不存在"}))).into_response(),
    }
}

// ============================================================
// 音乐上传
// ============================================================

const ALLOWED_EXTS: &[&str] = &["mp3","wav","flac","m4a","ogg","wma","ape"];

async fn music_upload(
    State(s): State<AppStateArc>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> impl IntoResponse {
    if auth_user_id(&headers, &s.config.jwt_secret).is_none() { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); }

    let mut file_data = Vec::new();
    let mut file_name = String::new();
    let mut ext = String::new();
    while let Ok(Some(field)) = mp.next_field().await {
        if field.name() == Some("file") {
            file_name = field.file_name().unwrap_or("unknown").to_string();
            ext = std::path::Path::new(&file_name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            if !ALLOWED_EXTS.contains(&ext.as_str()) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": format!("不支持的文件格式: {}", ext)}))).into_response();
            }
            file_data = field.bytes().await.unwrap_or_default().to_vec();
            break;
        }
    }
    if file_data.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"error":"没有上传文件"}))).into_response(); }

    // MD5 查重
    let mut hasher = Md5::new(); hasher.update(&file_data); let file_md5 = hex::encode(hasher.finalize());

    if let Ok(Some(existing)) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE md5=? AND md5!='' AND deleted_at IS NULL")
        .bind(&file_md5).fetch_optional(&s.db).await {
        return (StatusCode::CONFLICT, Json(json!({"error":format!("文件重复: 与曲库中的「{}」MD5 相同",existing.title),"duplicate":true,"md5":file_md5}))).into_response();
    }

    // 元数据提取
    let meta = {
        let tmp = std::env::temp_dir().join(format!("mu-{}.{}", uuid::Uuid::new_v4(), ext));
        std::fs::write(&tmp, &file_data).ok();
        let m = s.metadata.extract(&tmp.to_string_lossy(), &file_name, file_data.len() as i64);
        let _ = std::fs::remove_file(&tmp);
        m
    };

    let oss_key = storage::generate_object_key(&file_name);

    // 先落库拿到 mid，再上传存储：存储失败时删除记录回滚，避免孤儿文件/半写状态
    let max_sort: (Option<i32>,) = sqlx::query_as("SELECT MAX(sort_order) FROM musics WHERE deleted_at IS NULL").fetch_one(&s.db).await.unwrap_or((None,));
    let now = chrono::Utc::now().naive_utc();
    let r = match sqlx::query(
        "INSERT INTO musics (created_at,updated_at,title,album,genre,lyrics,duration,size,bitrate,sample_rate,channels,format,codec,channel_count,sort_order,oss_key,lyrics_key,cover_key,fingerprint,md5) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
    ).bind(now).bind(now).bind(&meta.title).bind(&meta.album).bind(&meta.genre).bind(&meta.lyrics)
     .bind(meta.duration).bind(meta.size).bind(meta.bitrate).bind(meta.sample_rate).bind(meta.channels)
     .bind(&meta.format).bind(&meta.codec).bind(meta.channels).bind(max_sort.0.unwrap_or(0)+1)
     .bind(&oss_key).bind("").bind("").bind("").bind(&file_md5)
     .execute(&s.db).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("入库失败: {}",e)}))).into_response(),
    };
    let mid = r.last_insert_rowid() as u32;

    if let Err(e) = s.storage.upload_file(&oss_key, file_data.clone()).await {
        let _ = sqlx::query("DELETE FROM musics WHERE id=?").bind(mid).execute(&s.db).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("存储上传失败: {}",e)}))).into_response();
    }

    // 歌手（存储上传成功后再创建）
    let mut artist_ids: Vec<u32> = Vec::new();
    for name in split_artists(&meta.artist).into_iter().take(10) {
        if let Ok(Some(a)) = sqlx::query_as::<_, Artist>("SELECT * FROM artists WHERE name=? AND deleted_at IS NULL").bind(&name).fetch_optional(&s.db).await {
            artist_ids.push(a.id);
        } else {
            let now = chrono::Utc::now().naive_utc();
            let r = sqlx::query("INSERT INTO artists (created_at,updated_at,name,description,avatar_url) VALUES (?,?,?,'','')")
                .bind(now).bind(now).bind(&name).execute(&s.db).await.unwrap();
            artist_ids.push(r.last_insert_rowid() as u32);
        }
    }

    // 封面上传（非致命，失败仅留空）
    let cover_key = if !meta.cover_data.is_empty() {
        let ext = image_mime_ext(&meta.cover_mime);
        let k = storage::generate_cover_key(&oss_key, ext);
        s.storage.upload_file(&k, meta.cover_data.clone()).await.ok().unwrap_or_default()
    } else { String::new() };

    // 歌词上传（非致命）
    let lyrics_key = if !meta.lyrics.is_empty() {
        let k = storage::generate_lyrics_key(&oss_key);
        s.storage.upload_file(&k, meta.lyrics.as_bytes().to_vec()).await.ok().unwrap_or_default()
    } else { String::new() };

    if !cover_key.is_empty() || !lyrics_key.is_empty() {
        let _ = sqlx::query("UPDATE musics SET cover_key=?, lyrics_key=? WHERE id=?").bind(&cover_key).bind(&lyrics_key).bind(mid).execute(&s.db).await;
    }

    // 关联歌手
    let now = chrono::Utc::now().naive_utc();
    for aid in &artist_ids { let _ = sqlx::query("INSERT INTO music_artists (music_id,artist_id,created_at) VALUES (?,?,?)").bind(mid).bind(aid).bind(now).execute(&s.db).await; }

    let music = match sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=?").bind(mid).fetch_one(&s.db).await {
        Ok(m) => m,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("读取上传结果失败: {}",e)}))).into_response(),
    };
    let artists = load_artists_for_music(&s.db, mid).await;
    let enriched = enrich(&music, &artists, true);

    (StatusCode::OK, Json(json!({"message":"上传成功","music":enriched}))).into_response()
}

async fn music_upload_batch(
    State(s): State<AppStateArc>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> impl IntoResponse {
    if auth_user_id(&headers, &s.config.jwt_secret).is_none() { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); }

    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new(); // (name, ext, data)
    let mut field_count = 0u32;
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                field_count += 1;
                let fname = field.name().unwrap_or("(none)").to_string();
                let ffile = field.file_name().unwrap_or("(none)").to_string();
                let fct = field.content_type().map(|m|m.to_string()).unwrap_or_else(||"(none)".into());
                log::warn!("[batch] field #{}, name={}, filename={}, content_type={}", field_count, fname, ffile, fct);
                if field.name() == Some("files") {
                    let name = field.file_name().unwrap_or("unknown").to_string();
                    let ext = std::path::Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    match field.bytes().await {
                        Ok(b) => {
                            log::warn!("[batch] 文件 {} 大小={} 字节", name, b.len());
                            files.push((name, ext, b.to_vec()));
                        }
                        Err(e) => {
                            log::error!("[batch] 读取文件 {} 失败: {:?}", name, e);
                        }
                    }
                } else {
                    match field.bytes().await {
                        Ok(b) => log::warn!("[batch] 跳过字段 #{} name={} size={}", field_count, fname, b.len()),
                        Err(e) => log::error!("[batch] 读取字段 #{} 失败: {:?}", field_count, e),
                    }
                }
            }
            Ok(None) => {
                log::warn!("[batch] multipart 流结束, 共 {} 个字段", field_count);
                break;
            }
            Err(e) => {
                log::error!("[batch] next_field() 错误: {:?}", e);
                break;
            }
        }
    }
    log::warn!("[batch] 总共收到 {} 个字段, {} 个文件", field_count, files.len());
    if files.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"error":"没有上传文件"}))).into_response(); }
    if files.len() > 3 { return (StatusCode::BAD_REQUEST, Json(json!({"error":"最多支持同时上传3个文件"}))).into_response(); }

    let mut results = Vec::new();
    for (name, ext, data) in files {
        if !ALLOWED_EXTS.contains(&ext.as_str()) {
            results.push(json!({"filename":name,"success":false,"error":format!("不支持的文件格式: {}",ext)})); continue;
        }
        let mut h = Md5::new(); h.update(&data); let md5 = hex::encode(h.finalize());
        if let Ok(Some(ex)) = sqlx::query_as::<_, Music>("SELECT id,title FROM musics WHERE md5=? AND md5!='' AND deleted_at IS NULL")
            .bind(&md5).fetch_optional(&s.db).await {
            results.push(json!({"filename":name,"success":false,"duplicate":true,"error":format!("文件重复: 与「{}」MD5 相同",ex.title)})); continue;
        }
        let tmp = std::env::temp_dir().join(format!("mu-{}.{}", uuid::Uuid::new_v4(), ext));
        std::fs::write(&tmp, &data).ok();
        let meta = s.metadata.extract(&tmp.to_string_lossy(), &name, data.len() as i64);
        let _ = std::fs::remove_file(&tmp);

        let ok = storage::generate_object_key(&name);
        let max: (Option<i32>,) = sqlx::query_as("SELECT MAX(sort_order) FROM musics WHERE deleted_at IS NULL").fetch_one(&s.db).await.unwrap_or((None,));
        let now = chrono::Utc::now().naive_utc();
        // 先落库再上传：上传失败时删除记录回滚
        let r = sqlx::query("INSERT INTO musics (created_at,updated_at,title,album,genre,lyrics,duration,size,bitrate,sample_rate,channels,format,codec,channel_count,sort_order,oss_key,lyrics_key,cover_key,fingerprint,md5) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(now).bind(now).bind(&meta.title).bind(&meta.album).bind(&meta.genre).bind(&meta.lyrics)
            .bind(meta.duration).bind(meta.size).bind(meta.bitrate).bind(meta.sample_rate).bind(meta.channels)
            .bind(&meta.format).bind(&meta.codec).bind(meta.channels).bind(max.0.unwrap_or(0)+1)
            .bind(&ok).bind("").bind("").bind("").bind(&md5)
            .execute(&s.db).await;
        let mid = match r {
            Ok(r) => r.last_insert_rowid() as u32,
            Err(e) => { results.push(json!({"filename":name,"success":false,"error":format!("入库失败: {}",e)})); continue; }
        };
        if let Err(e) = s.storage.upload_file(&ok, data).await {
            let _ = sqlx::query("DELETE FROM musics WHERE id=?").bind(mid).execute(&s.db).await;
            results.push(json!({"filename":name,"success":false,"error":format!("上传存储失败: {}",e)}));
            continue;
        }
        results.push(json!({"filename":name,"success":true}));
    }
    (StatusCode::OK, Json(json!({"message":"批量上传完成","results":results}))).into_response()
}

// ============================================================
// 音乐列表 / 详情 / 删除
// ============================================================

#[derive(Deserialize, Default)]
struct MusicListQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    keyword: Option<String>,
    genre: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    format: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
}

/// 将 music_list 的过滤条件以参数化方式追加到 QueryBuilder（防 SQL 注入）
fn push_music_list_conditions(qb: &mut sqlx::query_builder::QueryBuilder<'_, sqlx::Sqlite>, q: &MusicListQuery) {
    qb.push("m.deleted_at IS NULL");
    let like = |v: &str| format!("%{}%", v);
    if let Some(ref k) = q.keyword { if !k.is_empty() {
        qb.push(" AND (m.title LIKE ").push_bind(like(k))
          .push(" OR m.album LIKE ").push_bind(like(k))
          .push(" OR a.name LIKE ").push_bind(like(k))
          .push(")");
    } }
    // 歌手过滤（使用 a.name，调用方需保证已 LEFT JOIN artists）
    if let Some(ref ar) = q.artist { if !ar.is_empty() { qb.push(" AND a.name LIKE ").push_bind(like(ar)); } }
    if let Some(ref g) = q.genre { if !g.is_empty() { qb.push(" AND m.genre LIKE ").push_bind(like(g)); } }
    if let Some(ref al) = q.album { if !al.is_empty() { qb.push(" AND m.album LIKE ").push_bind(like(al)); } }
    if let Some(ref f) = q.format { if !f.is_empty() { qb.push(" AND m.format = ").push_bind(f.clone()); } }
}

async fn music_list(State(s): State<AppStateArc>, Query(q): Query<MusicListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(20).clamp(1, 100);
    let offset = (page - 1) * page_size;
    let sort_by = q.sort_by.clone().unwrap_or_else(|| "created_at".into());
    let order = q.sort_order.clone().unwrap_or_else(|| "desc".into());

    let need_join = q.keyword.as_ref().map(|s|!s.is_empty()).unwrap_or(false) || q.artist.as_ref().map(|s|!s.is_empty()).unwrap_or(false);
    let join = if need_join { " LEFT JOIN music_artists ma ON ma.music_id=m.id LEFT JOIN artists a ON a.id=ma.artist_id" } else { "" };

    // 排序字段与方向均走白名单，杜绝注入
    let order_dir = if order.eq_ignore_ascii_case("asc") { "ASC" } else { "DESC" };
    let order_clause = match sort_by.as_str() {
        "title" => format!("m.title {}", order_dir),
        "duration" => format!("m.duration {}", order_dir),
        "bitrate" => format!("m.bitrate {}", order_dir),
        "custom" => "m.sort_order ASC, m.created_at DESC".to_string(),
        _ => format!("m.created_at {}", order_dir),
    };

    // 数据查询（参数化）
    let select_sql = if need_join { "SELECT DISTINCT m.* FROM musics m" } else { "SELECT m.* FROM musics m" };
    let mut qb = sqlx::query_builder::QueryBuilder::<sqlx::Sqlite>::new(format!("{select_sql}{join} WHERE "));
    push_music_list_conditions(&mut qb, &q);
    qb.push(format!(" ORDER BY {order_clause} LIMIT "))
      .push_bind(page_size)
      .push(" OFFSET ")
      .push_bind(offset);
    let musics = qb.build_query_as::<Music>().fetch_all(&s.db).await.unwrap_or_default();

    // 计数查询（同样的参数化条件）
    let count_select = if need_join { "SELECT COUNT(DISTINCT m.id) FROM musics m" } else { "SELECT COUNT(m.id) FROM musics m" };
    let mut cq = sqlx::query_builder::QueryBuilder::<sqlx::Sqlite>::new(format!("{count_select}{join} WHERE "));
    push_music_list_conditions(&mut cq, &q);
    let (total,): (i64,) = cq.build_query_as::<(i64,)>().fetch_one(&s.db).await.unwrap_or((0,));
    let total_pages = (total + page_size - 1) / page_size;

    let mut enriched = Vec::new();
    for m in &musics {
        let artists = load_artists_for_music(&s.db, m.id).await;
        enriched.push(enrich(m, &artists, false));
    }

    Json(json!({"data":enriched,"pagination":{"page":page,"page_size":page_size,"total":total,"total_pages":total_pages}})).into_response()
}

async fn music_detail(State(s): State<AppStateArc>, Path(id): Path<u32>) -> impl IntoResponse {
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    let artists = load_artists_for_music(&s.db, id).await;
    Json(json!({"music":enrich(&m, &artists, true)})).into_response()
}

async fn music_delete(State(s): State<AppStateArc>, Path(id): Path<u32>) -> impl IntoResponse {
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    let _ = s.storage.delete_file(&m.oss_key).await;
    if !m.cover_key.is_empty() { let _ = s.storage.delete_file(&m.cover_key).await; }
    if !m.lyrics_key.is_empty() { let _ = s.storage.delete_file(&m.lyrics_key).await; }
    let _ = sqlx::query("DELETE FROM music_artists WHERE music_id=?").bind(id).execute(&s.db).await;
    let _ = sqlx::query("DELETE FROM playlist_musics WHERE music_id=?").bind(id).execute(&s.db).await;
    let now = chrono::Utc::now().naive_utc();
    let _ = sqlx::query("UPDATE musics SET deleted_at=? WHERE id=?").bind(now).bind(id).execute(&s.db).await;
    Json(json!({"message":"删除成功"})).into_response()
}

// ============================================================
// 流媒体 / 下载 / 封面 / 歌词
// ============================================================

/// 解析 Range 头，返回 (start, end)（含端点）。无效或不可满足时返回 Err，调用方应回 416。
fn parse_range(header: &str, file_size: i64) -> Result<(i64, i64), String> {
    if file_size <= 0 { return Err("空文件".into()); }
    let v = header.strip_prefix("bytes=").ok_or("无效Range")?;
    let v = v.split(',').next().unwrap_or("").trim();
    if let Some(s) = v.strip_prefix('-') {
        // 后缀范围 bytes=-N：最后 N 字节
        let n: i64 = s.trim().parse().map_err(|_|"无效")?;
        if n <= 0 { return Err("无效后缀范围".into()); }
        return Ok(((file_size - n).max(0), file_size - 1));
    }
    let (a, b) = v.split_once('-').ok_or("无效Range")?;
    let start: i64 = a.trim().parse().map_err(|_|"无效")?;
    if start < 0 || start >= file_size { return Err("起始位置越界".into()); }
    let end: i64 = if b.trim().is_empty() { file_size - 1 } else { b.trim().parse().map_err(|_|"无效")? };
    let end = end.min(file_size - 1);
    if end < start { return Err("结束位置小于起始位置".into()); }
    Ok((start, end))
}

fn fmt_ct(format: &str) -> &str {
    match format { "mp3"=>"audio/mpeg","flac"=>"audio/flac","wav"=>"audio/wav","m4a"|"aac"=>"audio/mp4","ogg"=>"audio/ogg","wma"=>"audio/x-ms-wma","ape"=>"audio/x-ape", _=>"application/octet-stream" }
}

async fn stream_music(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap) -> impl IntoResponse {
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    let ct = fmt_ct(&m.format);

    if let Some(range_val) = headers.get("range").and_then(|v| v.to_str().ok()) {
        let (start, end) = match parse_range(range_val, m.size) {
            Ok(r) => r,
            Err(_) => return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", m.size))],
                Json(json!({"error":"Range无效"})),
            ).into_response(),
        };
        match s.storage.get_file_range(&m.oss_key, start, end).await {
            Ok(data) => {
                (StatusCode::PARTIAL_CONTENT, [
                    (header::CONTENT_TYPE, ct.to_string()),
                    (header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end, m.size)),
                    (header::CONTENT_LENGTH, (end - start + 1).to_string()),
                    (header::ACCEPT_RANGES, "bytes".into()),
                    (header::CACHE_CONTROL, "public, max-age=3600".into()),
                ], data).into_response()
            }
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取文件失败"}))).into_response(),
        }
    } else {
        match s.storage.get_file_content(&m.oss_key).await {
            Ok(data) => ([
                (header::CONTENT_TYPE, ct.to_string()),
                (header::CONTENT_LENGTH, m.size.to_string()),
                (header::ACCEPT_RANGES, "bytes".into()),
                (header::CACHE_CONTROL, "public, max-age=3600".into()),
            ], data).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取文件失败"}))).into_response(),
        }
    }
}

async fn serve_cover(State(s): State<AppStateArc>, Path(id): Path<u32>) -> impl IntoResponse {
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    if m.cover_key.is_empty() { return (StatusCode::NOT_FOUND, Json(json!({"error":"该歌曲没有封面"}))).into_response(); }
    match s.storage.get_file_content(&m.cover_key).await {
        Ok(data) => {
            let ext = std::path::Path::new(&m.cover_key).extension().and_then(|e| e.to_str()).unwrap_or("jpg");
            let ct = match ext { "png"=>"image/png","gif"=>"image/gif","webp"=>"image/webp", _=>"image/jpeg" };
            ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "public, max-age=86400")], data).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取封面失败"}))).into_response(),
    }
}

async fn serve_lyrics(State(s): State<AppStateArc>, Path(id): Path<u32>) -> impl IntoResponse {
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    // 优先从存储读取
    if !m.lyrics_key.is_empty() {
        if let Ok(data) = s.storage.get_file_content(&m.lyrics_key).await {
            return ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=86400")], data).into_response();
        }
    }
    if !m.lyrics.is_empty() { return ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], m.lyrics.into_bytes()).into_response(); }
    (StatusCode::NOT_FOUND, Json(json!({"error":"该歌曲没有歌词"}))).into_response()
}

async fn music_proxy_download(State(s): State<AppStateArc>, Path(id): Path<u32>) -> impl IntoResponse {
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    match s.storage.get_file_content(&m.oss_key).await {
        Ok(data) => {
            let filename = format!("{}.{}", m.title, m.format);
            ([
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename*=UTF-8''{}", url_encode(&filename))),
                (HeaderName::from_static("content-transfer-encoding"), "binary".into()),
                (header::CACHE_CONTROL, "no-cache".into()),
                (HeaderName::from_static("x-music-title"), m.title),
            ], data).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取文件失败"}))).into_response(),
    }
}

// ============================================================
// 搜索建议 / 指纹 / 歌手 / 专辑
// ============================================================

async fn music_suggestions(State(s): State<AppStateArc>, Query(q): Query<MusicListQuery>) -> impl IntoResponse {
    let k = match q.keyword { Some(ref k) if !k.is_empty() => k.clone(), _ => return (StatusCode::BAD_REQUEST, Json(json!({"error":"缺少搜索关键词"}))).into_response() };
    let like = format!("%{}%", k);
    let titles: Vec<String> = sqlx::query_scalar("SELECT DISTINCT title FROM musics WHERE title LIKE ? AND deleted_at IS NULL LIMIT 5").bind(&like).fetch_all(&s.db).await.unwrap_or_default();
    #[derive(sqlx::FromRow, serde::Serialize)] struct As { id: u32, name: String }
    let artists = sqlx::query_as::<_, As>("SELECT id,name FROM artists WHERE name LIKE ? AND deleted_at IS NULL LIMIT 5").bind(&like).fetch_all(&s.db).await.unwrap_or_default();
    let albums: Vec<String> = sqlx::query_scalar("SELECT DISTINCT album FROM musics WHERE album LIKE ? AND deleted_at IS NULL LIMIT 5").bind(&like).fetch_all(&s.db).await.unwrap_or_default();
    Json(json!({"titles":titles,"artists":artists,"albums":albums})).into_response()
}

async fn music_fingerprint_check(
    State(s): State<AppStateArc>,
    headers: HeaderMap,
    Json(body): Json<FingerprintCheckRequest>,
) -> impl IntoResponse {
    if auth_user_id(&headers, &s.config.jwt_secret).is_none() { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); }
    let tol = body.duration_tolerance.unwrap_or(10.0).max(0.001);
    let min_sim = body.min_similarity.unwrap_or(0.85).clamp(0.0, 1.0);

    // 加载全部候选
    let candidates: Vec<(u32, f64, String)> = sqlx::query_as("SELECT id,duration,fingerprint FROM musics WHERE fingerprint!='' AND fingerprint IS NOT NULL AND deleted_at IS NULL")
        .fetch_all(&s.db).await.unwrap_or_default();

    let mut results = Vec::new();
    for (i, q) in body.queries.iter().enumerate() {
        let Ok(qfp) = fingerprint::decode_fingerprint(&q.fingerprint) else {
            results.push(FingerprintCheckResult { query_index: i, matched: false, similarity: 0.0, music: None }); continue;
        };
        let mut best_id = 0u32;
        let mut best_sim = 0.0f64;
        for (cid, cdur, cfp_str) in &candidates {
            if (cdur - q.duration).abs() > tol { continue; }
            if let Ok(cfp) = fingerprint::decode_fingerprint(cfp_str) {
                let sim = fingerprint::similarity(&qfp, &cfp);
                if sim > best_sim { best_sim = sim; }
                if sim >= min_sim { best_id = *cid; break; }
            }
        }
        if best_id > 0 {
            let m = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=?").bind(best_id).fetch_optional(&s.db).await.ok().flatten();
            let arts = load_artists_for_music(&s.db, best_id).await;
            let enriched = m.map(|mm| enrich(&mm, &arts, false));
            results.push(FingerprintCheckResult { query_index: i, matched: true, similarity: (best_sim*10000.0).round()/10000.0, music: enriched });
        } else {
            results.push(FingerprintCheckResult { query_index: i, matched: false, similarity: 0.0, music: None });
        }
    }
    Json(json!({"results":results})).into_response()
}

async fn list_fingerprints(State(s): State<AppStateArc>) -> impl IntoResponse {
    let musics = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE deleted_at IS NULL ORDER BY created_at DESC").fetch_all(&s.db).await.unwrap_or_default();
    let items: Vec<Value> = musics.iter().map(|m| json!({
        "id":m.id,"title":m.title,"album":m.album,"genre":m.genre,"duration":m.duration,"size":m.size,
        "bitrate":m.bitrate,"sample_rate":m.sample_rate,"channels":m.channels,"format":m.format,"codec":m.codec,
        "fingerprint":m.fingerprint,"song_url":format!("/api/v1/music/{}/stream",m.id)
    })).collect();
    Json(json!({"data":items,"total":items.len()})).into_response()
}

async fn artists_list(State(s): State<AppStateArc>, Query(q): Query<MusicListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1);
    let ps = q.page_size.unwrap_or(50).clamp(1, 200);
    let off = (page-1)*ps;
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artists WHERE deleted_at IS NULL").fetch_one(&s.db).await.unwrap_or((0,));
    let artists = sqlx::query_as::<_, Artist>("SELECT * FROM artists WHERE deleted_at IS NULL ORDER BY name ASC LIMIT ? OFFSET ?")
        .bind(ps).bind(off).fetch_all(&s.db).await.unwrap_or_default();
    Json(json!({"data":artists,"pagination":{"page":page,"page_size":ps,"total":total}})).into_response()
}

async fn artist_detail(State(s): State<AppStateArc>, Path(id): Path<u32>) -> impl IntoResponse {
    let Some(a) = sqlx::query_as::<_, Artist>("SELECT * FROM artists WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.unwrap_or(None)
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌手不存在"}))).into_response() };
    let musics = sqlx::query_as::<_, Music>("SELECT m.* FROM musics m INNER JOIN music_artists ma ON m.id=ma.music_id WHERE ma.artist_id=? AND m.deleted_at IS NULL").bind(id).fetch_all(&s.db).await.unwrap_or_default();
    let mut enriched_musics = Vec::new();
    for m in &musics {
        let artists = load_artists_for_music(&s.db, m.id).await;
        enriched_musics.push(enrich(m, &artists, false));
    }
    Json(json!({"artist":{"id":a.id,"name":a.name,"description":a.description,"avatar_url":a.avatar_url,"created_at":a.created_at,"updated_at":a.updated_at,"musics":enriched_musics}})).into_response()
}

async fn albums_list(State(s): State<AppStateArc>, Query(q): Query<MusicListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1);
    let ps = q.page_size.unwrap_or(50).clamp(1, 200);
    let off = (page-1)*ps;

    // SQLite 特性：使用 MAX() 聚合时，裸列 (id) 取自 cover_key 最大的那一行，
    // 因此 music_id 就是持有该封面的歌曲 id
    #[derive(sqlx::FromRow, serde::Serialize)] struct AR { name: String, cover_key: Option<String>, music_id: u32, track_count: i64 }
    let rows = sqlx::query_as::<_, AR>("SELECT album AS name, MAX(cover_key) AS cover_key, id AS music_id, COUNT(*) AS track_count FROM musics WHERE album!='' AND album IS NOT NULL AND deleted_at IS NULL GROUP BY album ORDER BY album ASC LIMIT ? OFFSET ?")
        .bind(ps).bind(off).fetch_all(&s.db).await.unwrap_or_default();
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(DISTINCT album) FROM musics WHERE album!='' AND album IS NOT NULL AND deleted_at IS NULL").fetch_one(&s.db).await.unwrap_or((0,));

    let items: Vec<Value> = rows.iter().map(|r| {
        let mut j = json!({"name":r.name,"track_count":r.track_count});
        let has_cover = r.cover_key.as_ref().map(|c|!c.is_empty()).unwrap_or(false);
        if has_cover {
            j["cover_url"] = json!(format!("/api/v1/music/{}/cover", r.music_id));
        }
        j
    }).collect();
    Json(json!({"data":items,"pagination":{"page":page,"page_size":ps,"total":total}})).into_response()
}

// ============================================================
// 分享
// ============================================================

async fn music_share(State(s): State<AppStateArc>, Path(mid): Path<u32>, headers: HeaderMap) -> impl IntoResponse {
    if auth_user_id(&headers, &s.config.jwt_secret).is_none() { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); }
    let (exists,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM musics WHERE id=? AND deleted_at IS NULL").bind(mid).fetch_one(&s.db).await.unwrap_or((0,));
    if exists == 0 { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response(); }
    let token = uuid::Uuid::new_v4().to_string().replace("-", "");
    let expires = chrono::Utc::now().naive_utc() + chrono::Duration::hours(s.config.share_expiry_hours);
    let now = chrono::Utc::now().naive_utc();
    sqlx::query("INSERT INTO shares (created_at,updated_at,music_id,token,expires_at) VALUES (?,?,?,?,?)")
        .bind(now).bind(now).bind(mid).bind(&token).bind(expires).execute(&s.db).await.unwrap();
    let host = headers.get("host").and_then(|v|v.to_str().ok()).unwrap_or("localhost:8080");
    let proto = if headers.get("x-forwarded-proto").map_or(false,|v|v.as_bytes()==b"https") {"https"} else {"http"};
    let share_url = format!("{}://{}/api/v1/shared/{}", proto, host, token);
    Json(json!({"share_url":share_url,"token":token,"expires_at":expires})).into_response()
}

/// HTML 转义：防止标题/歌词等用户内容注入分享页（存储型 XSS）
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
     .replace('\'', "&#39;")
}

fn share_page_html(title:&str, desc:&str, album:&str, stream:&str, cover:&str, page_url:&str, lyrics:&str) -> String {
    let (title, desc, album) = (html_escape(title), html_escape(desc), html_escape(album));
    let (stream, cover, page_url) = (html_escape(stream), html_escape(cover), html_escape(page_url));
    let cover_html = if !cover.is_empty() {
        format!(r#"<img class="cover" src="{}" alt="封面" onerror="this.onerror=null;this.outerHTML='<div class=\\'cover placeholder\\'>&#127925;</div>'">"#, cover)
    } else { r#"<div class="cover placeholder">🎵</div>"#.to_string() };
    let lrc = if !lyrics.is_empty() { format!(r#"<div class="lyrics"><pre>{}</pre></div>"#, html_escape(lyrics)) } else { String::new() };
    format!(r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1.0"><title>{title} — 墨迹音乐</title><meta property="og:title" content="{title}"><meta property="og:description" content="{desc}"><meta property="og:url" content="{page_url}"><style>*{{margin:0;padding:0;box-sizing:border-box}}body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;background:linear-gradient(135deg,#1a1a2e 0%,#16213e 50%,#0f3460 100%);min-height:100vh;display:flex;align-items:center;justify-content:center;padding:20px}}.player{{background:rgba(255,255,255,0.08);backdrop-filter:blur(20px);border-radius:24px;padding:32px 28px;width:90%;max-width:420px;text-align:center;box-shadow:0 20px 60px rgba(0,0,0,0.3)}}.cover{{width:200px;height:200px;border-radius:16px;object-fit:cover;margin:0 auto 24px;display:block;background:rgba(255,255,255,0.1)}}.cover.placeholder{{display:flex;align-items:center;justify-content:center;font-size:4rem}}.title{{color:#fff;font-size:1.4rem;font-weight:700;margin-bottom:8px}}.artist{{color:rgba(255,255,255,0.6);font-size:.95rem;margin-bottom:4px}}.album{{color:rgba(255,255,255,0.4);font-size:.85rem;margin-bottom:24px}}audio{{width:100%;margin-bottom:16px;border-radius:12px}}.lyrics{{max-height:200px;overflow-y:auto;margin-top:16px;padding:12px;background:rgba(255,255,255,0.05);border-radius:12px;text-align:left}}.lyrics pre{{color:rgba(255,255,255,0.7);font-size:.85rem;line-height:1.8;white-space:pre-wrap;font-family:inherit}}.footer{{color:rgba(255,255,255,0.3);font-size:.75rem;margin-top:16px}}</style></head><body><div class="player">{cover_html}<div class="title">{title}</div><div class="artist">{desc}</div><div class="album">{album}</div><audio controls><source src="{stream}" type="audio/mpeg"></audio>{lrc}<div class="footer">Powered by 墨迹音乐</div></div></body></html>"#)
}

async fn share_page(State(s): State<AppStateArc>, Path(token): Path<String>, headers: HeaderMap) -> impl IntoResponse {
    let Some(sh) = sqlx::query_as::<_, Share>("SELECT * FROM shares WHERE token=? AND deleted_at IS NULL").bind(&token).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, [("content-type","text/html; charset=utf-8")], "<h1>链接不存在</h1>".to_string()).into_response() };
    if chrono::Utc::now().naive_utc() > sh.expires_at {
        return (StatusCode::GONE, [("content-type","text/html; charset=utf-8")], "<h1>分享链接已过期</h1>".to_string()).into_response();
    }
    let music = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(sh.music_id).fetch_optional(&s.db).await.ok().flatten().unwrap_or_default();
    let artists = load_artists_for_music(&s.db, sh.music_id).await;
    let names: Vec<String> = artists.iter().map(|a|a.name.clone()).collect();
    let artist_str = if names.is_empty() { "未知艺术家".into() } else { names.join(", ") };
    let album = if music.album.is_empty() { "未知专辑" } else { &music.album };
    let desc = format!("{} · {}", artist_str, album);

    let host = headers.get("host").and_then(|v|v.to_str().ok()).unwrap_or("localhost:8080");
    let proto = if headers.get("x-forwarded-proto").map_or(false,|v|v.as_bytes()==b"https") {"https"} else {"http"};
    let base = format!("{}://{}", proto, host);
    let stream_url = format!("{}/api/v1/shared/{}/stream", base, token);
    let cover_url = if !music.cover_key.is_empty() { format!("{}/api/v1/shared/{}/cover", base, token) } else { String::new() };
    let page_url = format!("{}/api/v1/shared/{}", base, token);

    let html = share_page_html(&music.title, &desc, album, &stream_url, &cover_url, &page_url, &music.lyrics);
    (StatusCode::OK, [("content-type","text/html; charset=utf-8")], html).into_response()
}

async fn share_stream(State(s): State<AppStateArc>, Path(token): Path<String>, headers: HeaderMap) -> impl IntoResponse {
    let Some(sh) = sqlx::query_as::<_, Share>("SELECT * FROM shares WHERE token=? AND deleted_at IS NULL").bind(&token).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"分享不存在"}))).into_response() };
    if chrono::Utc::now().naive_utc() > sh.expires_at { return (StatusCode::GONE, Json(json!({"error":"分享已过期"}))).into_response(); }

    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(sh.music_id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    let ct = fmt_ct(&m.format);

    if let Some(rv) = headers.get("range").and_then(|v|v.to_str().ok()) {
        let (start, end) = match parse_range(rv, m.size) {
            Ok(r)=>r,
            Err(_)=>return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{}", m.size))], Json(json!({"error":"Range无效"}))).into_response(),
        };
        match s.storage.get_file_range(&m.oss_key, start, end).await {
            Ok(d) => (StatusCode::PARTIAL_CONTENT, [ (header::CONTENT_TYPE,ct.to_string()), (header::CONTENT_RANGE,format!("bytes {}-{}/{}",start,end,m.size)), (header::CONTENT_LENGTH,(end-start+1).to_string()), (header::ACCEPT_RANGES,"bytes".into()), (header::CACHE_CONTROL,"public, max-age=3600".into()) ], d).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取失败"}))).into_response(),
        }
    } else {
        match s.storage.get_file_content(&m.oss_key).await {
            Ok(d) => ([(header::CONTENT_TYPE,ct.to_string()),(header::CONTENT_LENGTH,m.size.to_string()),(header::ACCEPT_RANGES,"bytes".into()),(header::CACHE_CONTROL,"public, max-age=3600".into())], d).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取失败"}))).into_response(),
        }
    }
}

async fn share_cover(State(s): State<AppStateArc>, Path(token): Path<String>) -> impl IntoResponse {
    let Some(sh) = sqlx::query_as::<_, Share>("SELECT * FROM shares WHERE token=? AND deleted_at IS NULL").bind(&token).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"分享不存在"}))).into_response() };
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(sh.music_id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response() };
    if m.cover_key.is_empty() { return (StatusCode::NOT_FOUND, Json(json!({"error":"没有封面"}))).into_response(); }
    match s.storage.get_file_content(&m.cover_key).await {
        Ok(d) => { let ext = std::path::Path::new(&m.cover_key).extension().and_then(|e|e.to_str()).unwrap_or("jpg"); let ct = match ext {"png"=>"image/png","gif"=>"image/gif","webp"=>"image/webp",_=>"image/jpeg"}; ([(header::CONTENT_TYPE,ct),(header::CACHE_CONTROL,"public, max-age=86400")], d).into_response() }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"获取封面失败"}))).into_response(),
    }
}

// ============================================================
// 歌单
// ============================================================

#[derive(Deserialize)] struct PlCreate { name: String, #[serde(default)] description: String, #[serde(default)] cover_url: String }
#[derive(Deserialize)] struct PlUpdate { name: Option<String>, description: Option<String>, cover_url: Option<String> }
#[derive(Deserialize)] struct PlAddMusic { music_id: u32 }
#[derive(Deserialize, Default)] struct PlQuery { page: Option<i64>, page_size: Option<i64>, user_id: Option<u32> }

async fn playlist_create(State(s): State<AppStateArc>, headers: HeaderMap, Json(b): Json<PlCreate>) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    if b.name.trim().is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"error":"歌单名不能为空"}))).into_response(); }
    let now = chrono::Utc::now().naive_utc();
    let r = sqlx::query("INSERT INTO playlists (created_at,updated_at,name,description,cover_url,user_id) VALUES (?,?,?,?,?,?)")
        .bind(now).bind(now).bind(b.name.trim()).bind(&b.description).bind(&b.cover_url).bind(uid).execute(&s.db).await.unwrap();
    Json(json!({"message":"创建成功","playlist":{"id":r.last_insert_rowid() as u32,"name":b.name,"description":b.description,"cover_url":b.cover_url,"user_id":uid}})).into_response()
}

async fn playlists_list(State(s): State<AppStateArc>, Query(q): Query<PlQuery>, headers: HeaderMap) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1); let ps = q.page_size.unwrap_or(20).clamp(1,100); let off = (page-1)*ps;
    let uid = auth_user_id(&headers, &s.config.jwt_secret);
    let filter_uid = q.user_id.or(uid);
    let has_filter = filter_uid.is_some();

    let (pls, total): (Vec<Playlist>, i64) = if let Some(fid) = filter_uid {
        let list = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE deleted_at IS NULL AND user_id=? ORDER BY created_at DESC LIMIT ? OFFSET ?")
            .bind(fid).bind(ps).bind(off).fetch_all(&s.db).await.unwrap_or_default();
        let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM playlists WHERE deleted_at IS NULL AND user_id=?").bind(fid).fetch_one(&s.db).await.unwrap_or((0,));
        (list, c)
    } else {
        let list = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE deleted_at IS NULL ORDER BY created_at DESC LIMIT ? OFFSET ?")
            .bind(ps).bind(off).fetch_all(&s.db).await.unwrap_or_default();
        let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM playlists WHERE deleted_at IS NULL").fetch_one(&s.db).await.unwrap_or((0,));
        (list, c)
    };
    Json(json!({"data":pls,"pagination":{"page":page,"page_size":ps,"total":total,"total_pages":(total+ps-1)/ps}})).into_response()
}

async fn playlist_detail(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap) -> impl IntoResponse {
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response() };
    if let Some(uid) = auth_user_id(&headers, &s.config.jwt_secret) { if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权访问"}))).into_response(); } }

    let pms = sqlx::query_as::<_, PlaylistMusic>("SELECT * FROM playlist_musics WHERE playlist_id=? ORDER BY sort_order ASC, added_at DESC").bind(id).fetch_all(&s.db).await.unwrap_or_default();
    let mut ids: Vec<u32> = pms.iter().map(|pm|pm.music_id).collect(); ids.dedup();
    let mut enriched_musics = Vec::new();
    if !ids.is_empty() {
        let ph: Vec<String> = ids.iter().map(|_|"?".to_string()).collect();
        let musics = sqlx::query_as::<_, Music>(&format!("SELECT * FROM musics WHERE id IN ({}) AND deleted_at IS NULL", ph.join(","))).fetch_all(&s.db).await.unwrap_or_default();
        for m in &musics {
            let artists = load_artists_for_music(&s.db, m.id).await;
            enriched_musics.push(enrich(m, &artists, false));
        }
    }
    Json(json!({"playlist":{"id":pl.id,"name":pl.name,"description":pl.description,"cover_url":pl.cover_url,"user_id":pl.user_id,"created_at":pl.created_at,"musics":enriched_musics}})).into_response()
}

async fn playlist_update(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap, Json(b): Json<PlUpdate>) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response() };
    if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权修改"}))).into_response(); }
    // 参数化 UPDATE：SET 子句与绑定值按固定顺序一一对应（防 SQL 注入）
    let mut sets: Vec<&str> = Vec::new();
    let mut v_name: Option<String> = None;
    let mut v_desc: Option<String> = None;
    let mut v_cover: Option<String> = None;
    if let Some(ref n) = b.name { if !n.is_empty() { sets.push("name=?"); v_name = Some(n.clone()); } }
    if let Some(ref d) = b.description { sets.push("description=?"); v_desc = Some(d.clone()); }
    if let Some(ref c) = b.cover_url { sets.push("cover_url=?"); v_cover = Some(c.clone()); }
    if !sets.is_empty() {
        sets.push("updated_at=?");
        let sql = format!("UPDATE playlists SET {} WHERE id=?", sets.join(", "));
        let mut upd = sqlx::query(&sql);
        if let Some(ref v) = v_name { upd = upd.bind(v); }
        if let Some(ref v) = v_desc { upd = upd.bind(v); }
        if let Some(ref v) = v_cover { upd = upd.bind(v); }
        upd = upd.bind(chrono::Utc::now().naive_utc()).bind(id);
        if let Err(e) = upd.execute(&s.db).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("更新失败: {}",e)}))).into_response();
        }
    }
    Json(json!({"message":"更新成功"})).into_response()
}

async fn playlist_delete(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response() };
    if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权删除"}))).into_response(); }
    sqlx::query("DELETE FROM playlist_musics WHERE playlist_id=?").bind(id).execute(&s.db).await.unwrap();
    let now = chrono::Utc::now().naive_utc();
    sqlx::query("UPDATE playlists SET deleted_at=? WHERE id=?").bind(now).bind(id).execute(&s.db).await.unwrap();
    Json(json!({"message":"删除成功"})).into_response()
}

async fn playlist_add_music(State(s): State<AppStateArc>, Path(pid): Path<u32>, headers: HeaderMap, Json(b): Json<PlAddMusic>) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(pid).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response() };
    if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权修改"}))).into_response(); }
    // 去重检查
    let (cnt,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM playlist_musics WHERE playlist_id=? AND music_id=?").bind(pid).bind(b.music_id).fetch_one(&s.db).await.unwrap_or((0,));
    if cnt > 0 { return (StatusCode::BAD_REQUEST, Json(json!({"error":"歌曲已在歌单中"}))).into_response(); }
    let (max_sort,): (Option<i32>,) = sqlx::query_as("SELECT MAX(sort_order) FROM playlist_musics WHERE playlist_id=?").bind(pid).fetch_one(&s.db).await.unwrap_or((None,));
    let now = chrono::Utc::now().naive_utc();
    sqlx::query("INSERT INTO playlist_musics (playlist_id,music_id,added_at,sort_order) VALUES (?,?,?,?)")
        .bind(pid).bind(b.music_id).bind(now).bind(max_sort.unwrap_or(0)+1).execute(&s.db).await.unwrap();
    Json(json!({"message":"添加成功"})).into_response()
}

async fn playlist_remove_music(State(s): State<AppStateArc>, Path((pid, mid)): Path<(u32,u32)>, headers: HeaderMap) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response() };
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(pid).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response() };
    if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权修改"}))).into_response(); }
    sqlx::query("DELETE FROM playlist_musics WHERE playlist_id=? AND music_id=?").bind(pid).bind(mid).execute(&s.db).await.unwrap();
    Json(json!({"message":"移除成功"})).into_response()
}

// ============================================================
// 新增端点: update/reorder/artist-by-name/album-music/playlist-music/batch
// ============================================================

#[derive(Deserialize)] struct UpdateMusicReq { title: Option<String>, artists: Option<Vec<String>>, album: Option<String>, genre: Option<String>, lyrics: Option<String> }
#[derive(Deserialize)] struct ReorderReq { music_ids: Vec<u32> }
#[derive(Deserialize)] struct BatchDeleteReq { music_ids: Vec<u32> }

async fn music_update(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap, Json(b): Json<UpdateMusicReq>) -> impl IntoResponse {
    let Some(_uid) = auth_user_id(&headers, &s.config.jwt_secret) else { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); };
    let (exists,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_one(&s.db).await.unwrap_or((0,));
    if exists == 0 { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response(); }
    // 参数化 UPDATE：SET 子句与绑定值按固定顺序一一对应（防 SQL 注入）
    let mut sets: Vec<&str> = Vec::new();
    let mut v_title: Option<String> = None;
    let mut v_album: Option<String> = None;
    let mut v_genre: Option<String> = None;
    let mut v_lyrics: Option<String> = None;
    if let Some(ref t) = b.title { if !t.is_empty() { sets.push("title=?"); v_title = Some(t.clone()); } }
    if let Some(ref a) = b.album { sets.push("album=?"); v_album = Some(a.clone()); }
    if let Some(ref g) = b.genre { sets.push("genre=?"); v_genre = Some(g.clone()); }
    if let Some(ref l) = b.lyrics { sets.push("lyrics=?"); v_lyrics = Some(l.clone()); }
    if !sets.is_empty() {
        sets.push("updated_at=?");
        let sql = format!("UPDATE musics SET {} WHERE id=?", sets.join(", "));
        let mut upd = sqlx::query(&sql);
        if let Some(ref v) = v_title { upd = upd.bind(v); }
        if let Some(ref v) = v_album { upd = upd.bind(v); }
        if let Some(ref v) = v_genre { upd = upd.bind(v); }
        if let Some(ref v) = v_lyrics { upd = upd.bind(v); }
        upd = upd.bind(chrono::Utc::now().naive_utc()).bind(id);
        if let Err(e) = upd.execute(&s.db).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("更新失败: {}",e)}))).into_response();
        }
    }
    // Handle artists replacement
    if let Some(ref artists) = b.artists {
        sqlx::query("DELETE FROM music_artists WHERE music_id=?").bind(id).execute(&s.db).await.unwrap();
        let now = chrono::Utc::now().naive_utc();
        for name in artists {
            let aid: Option<(u32,)> = sqlx::query_as("SELECT id FROM artists WHERE name=? AND deleted_at IS NULL").bind(name).fetch_optional(&s.db).await.unwrap_or(None);
            let aid = if let Some((id,)) = aid { id } else {
                sqlx::query("INSERT INTO artists (created_at,updated_at,name,description,avatar_url) VALUES (?,?,?,'','')").bind(now).bind(now).bind(name).execute(&s.db).await.unwrap().last_insert_rowid() as u32
            };
            sqlx::query("INSERT INTO music_artists (music_id,artist_id,created_at) VALUES (?,?,?)").bind(id).bind(aid).bind(now).execute(&s.db).await.unwrap();
        }
    }
    // Fetch updated music with artists and return enriched
    let updated = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=?").bind(id).fetch_one(&s.db).await.unwrap();
    let artists = load_artists_for_music(&s.db, id).await;
    let enriched = enrich(&updated, &artists, true);
    Json(json!({"message":"更新成功","music":enriched})).into_response()
}

async fn music_update_cover(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap, mut mp: Multipart) -> impl IntoResponse {
    let Some(_uid) = auth_user_id(&headers, &s.config.jwt_secret) else { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); };
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response(); };
    let mut uploaded = None;
    while let Ok(Some(field)) = mp.next_field().await {
        let is_cover = field.name() == Some("cover");
        let fname = field.file_name().unwrap_or("cover.jpg").to_string();
        if is_cover {
            let ext = std::path::Path::new(&fname).extension().and_then(|e|e.to_str()).unwrap_or("jpg");
            let data = field.bytes().await.unwrap_or_default().to_vec();
            if !data.is_empty() {
                let key = storage::generate_cover_key(&m.oss_key, &format!(".{}", ext));
                s.storage.upload_file(&key, data).await.unwrap();
                if !m.cover_key.is_empty() && m.cover_key != key { let _ = s.storage.delete_file(&m.cover_key).await; }
                sqlx::query("UPDATE musics SET cover_key=?, updated_at=? WHERE id=?").bind(&key).bind(chrono::Utc::now().naive_utc()).bind(id).execute(&s.db).await.unwrap();
                uploaded = Some(key);
            }
            break;
        }
    }
    match uploaded { Some(_) => Json(json!({"message":"封面更新成功"})).into_response(), None => (StatusCode::BAD_REQUEST, Json(json!({"error":"没有上传封面文件"}))).into_response() }
}

async fn music_update_lyrics(State(s): State<AppStateArc>, Path(id): Path<u32>, headers: HeaderMap, Json(b): Json<Value>) -> impl IntoResponse {
    let Some(_uid) = auth_user_id(&headers, &s.config.jwt_secret) else { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); };
    let Some(m) = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE id=? AND deleted_at IS NULL").bind(id).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"音乐不存在"}))).into_response(); };
    let lyrics = b["lyrics"].as_str().unwrap_or("");
    let key = storage::generate_lyrics_key(&m.oss_key);
    s.storage.upload_file(&key, lyrics.as_bytes().to_vec()).await.unwrap();
    if !m.lyrics_key.is_empty() && m.lyrics_key != key { let _ = s.storage.delete_file(&m.lyrics_key).await; }
    sqlx::query("UPDATE musics SET lyrics=?, lyrics_key=?, updated_at=? WHERE id=?").bind(lyrics).bind(&key).bind(chrono::Utc::now().naive_utc()).bind(id).execute(&s.db).await.unwrap();
    Json(json!({"message":"歌词更新成功"})).into_response()
}

async fn music_reorder(State(s): State<AppStateArc>, headers: HeaderMap, Json(b): Json<ReorderReq>) -> impl IntoResponse {
    let Some(_uid) = auth_user_id(&headers, &s.config.jwt_secret) else { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); };
    for (i, mid) in b.music_ids.iter().enumerate() {
        sqlx::query("UPDATE musics SET sort_order=? WHERE id=?").bind((i+1) as i32).bind(mid).execute(&s.db).await.unwrap();
    }
    Json(json!({"message":"排序更新成功"})).into_response()
}

async fn artist_by_name(State(s): State<AppStateArc>, Path(name): Path<String>) -> impl IntoResponse {
    let Some(a) = sqlx::query_as::<_, Artist>("SELECT * FROM artists WHERE name=? AND deleted_at IS NULL").bind(&name).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"艺术家不存在"}))).into_response(); };
    let musics = sqlx::query_as::<_, Music>("SELECT m.* FROM musics m INNER JOIN music_artists ma ON m.id=ma.music_id WHERE ma.artist_id=? AND m.deleted_at IS NULL").bind(a.id).fetch_all(&s.db).await.unwrap_or_default();
    let mut enriched_musics = Vec::new();
    for m in &musics {
        let artists = load_artists_for_music(&s.db, m.id).await;
        enriched_musics.push(enrich(m, &artists, false));
    }
    Json(json!({"artist":{"id":a.id,"name":a.name,"description":a.description,"avatar_url":a.avatar_url,"created_at":a.created_at,"updated_at":a.updated_at,"musics":enriched_musics}})).into_response()
}

async fn album_music(State(s): State<AppStateArc>, Path(name): Path<String>, Query(q): Query<MusicListQuery>) -> impl IntoResponse {
    let page = q.page.unwrap_or(1).max(1); let ps = q.page_size.unwrap_or(200).clamp(1,500); let off = (page-1)*ps;
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM musics WHERE album=? AND deleted_at IS NULL").bind(&name).fetch_one(&s.db).await.unwrap_or((0,));
    let mut musics = sqlx::query_as::<_, Music>("SELECT * FROM musics WHERE album=? AND deleted_at IS NULL ORDER BY title ASC LIMIT ? OFFSET ?").bind(&name).bind(ps).bind(off).fetch_all(&s.db).await.unwrap_or_default();
    // 列表场景不输出全量歌词
    for m in &mut musics { m.lyrics = String::new(); }
    let cover_url = musics.first().and_then(|m| if m.cover_key.is_empty() {None} else {Some(format!("/api/v1/music/{}/cover", m.id))}).unwrap_or_default();
    Json(json!({"album":name,"cover_url":cover_url,"musics":musics,"total":total})).into_response()
}

async fn playlist_get_music(State(s): State<AppStateArc>, Path(pid): Path<u32>, headers: HeaderMap) -> impl IntoResponse {
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(pid).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response(); };
    if let Some(uid) = auth_user_id(&headers, &s.config.jwt_secret) { if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权访问"}))).into_response(); } }
    let pms = sqlx::query_as::<_, PlaylistMusic>("SELECT * FROM playlist_musics WHERE playlist_id=? ORDER BY sort_order ASC, added_at DESC").bind(pid).fetch_all(&s.db).await.unwrap_or_default();
    let mut ids: Vec<u32> = pms.iter().map(|pm|pm.music_id).collect(); ids.dedup();
    // Build sort_order map
    let sort_map: std::collections::HashMap<u32, i32> = pms.iter().map(|pm| (pm.music_id, pm.sort_order)).collect();
    let mut songs = Vec::new();
    if !ids.is_empty() {
        let ph: Vec<String> = ids.iter().map(|_|"?".to_string()).collect();
        let musics = sqlx::query_as::<_, Music>(&format!("SELECT * FROM musics WHERE id IN ({}) AND deleted_at IS NULL", ph.join(","))).fetch_all(&s.db).await.unwrap_or_default();
        for m in &musics {
            let artists = load_artists_for_music(&s.db, m.id).await;
            let mut song = enrich(m, &artists, false);
            if let Some(&so) = sort_map.get(&m.id) {
                song["sort_order"] = json!(so);
            }
            songs.push(song);
        }
    }
    Json(json!({"songs":songs,"total":songs.len()})).into_response()
}

async fn playlist_reorder_music(State(s): State<AppStateArc>, Path(pid): Path<u32>, headers: HeaderMap, Json(b): Json<ReorderReq>) -> impl IntoResponse {
    let Some(uid) = auth_user_id(&headers, &s.config.jwt_secret) else { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); };
    let Some(pl) = sqlx::query_as::<_, Playlist>("SELECT * FROM playlists WHERE id=? AND deleted_at IS NULL").bind(pid).fetch_optional(&s.db).await.ok().flatten()
        else { return (StatusCode::NOT_FOUND, Json(json!({"error":"歌单不存在"}))).into_response(); };
    if pl.user_id != uid { return (StatusCode::FORBIDDEN, Json(json!({"error":"无权修改"}))).into_response(); }
    for (i, mid) in b.music_ids.iter().enumerate() {
        sqlx::query("UPDATE playlist_musics SET sort_order=? WHERE playlist_id=? AND music_id=?").bind((i+1) as i32).bind(pid).bind(mid).execute(&s.db).await.unwrap();
    }
    Json(json!({"message":"排序更新成功"})).into_response()
}

async fn batch_delete_music(State(s): State<AppStateArc>, headers: HeaderMap, Json(b): Json<BatchDeleteReq>) -> impl IntoResponse {
    let Some(_uid) = auth_user_id(&headers, &s.config.jwt_secret) else { return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未认证"}))).into_response(); };
    if b.music_ids.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"error":"没有指定要删除的歌曲"}))).into_response(); }

    let placeholders = b.music_ids.iter().map(|_|"?").collect::<Vec<_>>().join(",");

    // 加载待删除歌曲（用于清理存储文件）
    let sql_sel = format!("SELECT * FROM musics WHERE id IN ({placeholders}) AND deleted_at IS NULL");
    let mut sel = sqlx::query_as::<_, Music>(&sql_sel);
    for id in &b.music_ids { sel = sel.bind(*id); }
    let musics = match sel.fetch_all(&s.db).await {
        Ok(m) => m,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("查询待删歌曲失败: {}",e)}))).into_response(),
    };

    for m in &musics {
        let _ = s.storage.delete_file(&m.oss_key).await;
        if !m.cover_key.is_empty() { let _ = s.storage.delete_file(&m.cover_key).await; }
        if !m.lyrics_key.is_empty() { let _ = s.storage.delete_file(&m.lyrics_key).await; }
    }

    // 按占位符顺序绑定所有 id
    let sql_art = format!("DELETE FROM music_artists WHERE music_id IN ({placeholders})");
    let mut del_artists = sqlx::query(&sql_art);
    let sql_pl = format!("DELETE FROM playlist_musics WHERE music_id IN ({placeholders})");
    let mut del_playlists = sqlx::query(&sql_pl);
    let sql_upd = format!("UPDATE musics SET deleted_at=? WHERE id IN ({placeholders}) AND deleted_at IS NULL");
    let mut soft_del = sqlx::query(&sql_upd);
    soft_del = soft_del.bind(chrono::Utc::now().naive_utc());
    for id in &b.music_ids {
        del_artists = del_artists.bind(*id);
        del_playlists = del_playlists.bind(*id);
        soft_del = soft_del.bind(*id);
    }

    if let Err(e) = del_artists.execute(&s.db).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("删除歌手关联失败: {}",e)}))).into_response();
    }
    if let Err(e) = del_playlists.execute(&s.db).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("删除歌单关联失败: {}",e)}))).into_response();
    }
    let deleted = match soft_del.execute(&s.db).await {
        Ok(r) => r.rows_affected() as usize,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("删除歌曲失败: {}",e)}))).into_response(),
    };

    Json(json!({"message": format!("成功删除 {} 首歌曲", deleted)})).into_response()
}

// ============================================================
// 设备同步
// ============================================================

async fn device_register(State(s): State<AppStateArc>, headers: HeaderMap, Json(b): Json<Value>) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response() };
    let did = b["device_id"].as_str().unwrap_or("").to_string();
    if did.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"error":"缺少 device_id"}))).into_response(); }
    let name = b["device_name"].as_str().unwrap_or(&did).to_string();
    let dtype = b["device_type"].as_str().unwrap_or("android").to_string();
    let role = b["role"].as_str().unwrap_or("slave").to_string();
    let sync = b["sync_enabled"].as_bool().unwrap_or(true);
    let ip = headers.get("x-forwarded-for").and_then(|v|v.to_str().ok()).unwrap_or("127.0.0.1").to_string();
    let now = chrono::Utc::now().naive_utc();

    let result = sqlx::query(
        "INSERT INTO devices (created_at,updated_at,user_id,device_id,device_name,device_type,ip_address,is_online,role,sync_enabled,last_seen) VALUES (?,?,?,?,?,?,?,1,?,?,?) ON CONFLICT(device_id,user_id) DO UPDATE SET device_name=excluded.device_name,ip_address=excluded.ip_address,is_online=1,role=excluded.role,sync_enabled=excluded.sync_enabled,last_seen=excluded.last_seen,updated_at=excluded.updated_at"
    ).bind(now).bind(now).bind(uid).bind(&did).bind(&name).bind(&dtype).bind(&ip).bind(&role).bind(sync).bind(now).execute(&s.db).await;

    match result {
        Ok(_) => Json(json!({"status":"ok"})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("注册设备失败: {}",e)}))).into_response(),
    }
}

async fn devices_list(State(s): State<AppStateArc>, headers: HeaderMap) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response() };
    let devices = sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE user_id=? ORDER BY is_online DESC, last_seen DESC").bind(uid).fetch_all(&s.db).await.unwrap_or_default();
    Json(json!({"devices":devices})).into_response()
}

async fn device_unregister(State(s): State<AppStateArc>, headers: HeaderMap, Path(did): Path<String>) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response() };
    let now = chrono::Utc::now().naive_utc();
    // 限定只能注销自己的设备
    match sqlx::query("UPDATE devices SET is_online=0,last_seen=?,updated_at=? WHERE device_id=? AND user_id=?").bind(now).bind(now).bind(&did).bind(uid).execute(&s.db).await {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"status":"ok"})).into_response(),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error":"设备不存在或无权操作"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("注销设备失败: {}",e)}))).into_response(),
    }
}

async fn sync_status(State(s): State<AppStateArc>, headers: HeaderMap) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response() };
    let devices = sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE user_id=? ORDER BY is_online DESC, last_seen DESC").bind(uid).fetch_all(&s.db).await.unwrap_or_default();
    let hub_clients = s.hub.get_user_clients(uid).await;
    let online: std::collections::HashSet<String> = hub_clients.iter().map(|c|c.device_id.clone()).collect();

    let mut host_did = String::new();
    let devs_json: Vec<Value> = devices.iter().map(|d| {
        let is_on = d.is_online && online.contains(&d.device_id);
        if d.role=="host" && is_on { host_did = d.device_id.clone(); }
        json!({"device_id":d.device_id,"device_name":d.device_name,"role":d.role,"sync_enabled":d.sync_enabled,"is_online":is_on,"last_seen":d.last_seen.format("%Y-%m-%d %H:%M:%S").to_string()})
    }).collect();
    Json(json!({"user_id":uid,"host_device_id":host_did,"devices":devs_json})).into_response()
}

async fn sync_toggle_slave(State(s): State<AppStateArc>, headers: HeaderMap, Json(b): Json<Value>) -> impl IntoResponse {
    let uid = match auth_user_id(&headers, &s.config.jwt_secret) { Some(id)=>id, None=>return (StatusCode::UNAUTHORIZED, Json(json!({"error":"unauthorized"}))).into_response() };
    let did = b["device_id"].as_str().unwrap_or("").to_string();
    let enabled = b["enabled"].as_bool().unwrap_or(false);
    let clients = s.hub.get_user_clients(uid).await;
    if !clients.iter().any(|c|c.role=="host") { return (StatusCode::FORBIDDEN, Json(json!({"error":"当前用户没有活跃的主机"}))).into_response(); }
    // 限定只能操作自己的设备
    let r = match sqlx::query("UPDATE devices SET sync_enabled=? WHERE device_id=? AND user_id=?").bind(enabled).bind(&did).bind(uid).execute(&s.db).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":format!("更新失败: {}",e)}))).into_response(),
    };
    if r.rows_affected() == 0 { return (StatusCode::NOT_FOUND, Json(json!({"error":"设备不存在或无权操作"}))).into_response(); }
    s.hub.set_slave_enabled(&did, enabled).await;
    (StatusCode::OK, Json(json!({"status":"ok"}))).into_response()
}

async fn ws_upgrade(State(s): State<AppStateArc>, ws: WebSocketUpgrade, Query(params): Query<Value>) -> impl IntoResponse {
    let token = params.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let device_id = params.get("device_id").and_then(|v| v.as_str()).unwrap_or("");
    let device_name = params.get("device_name").and_then(|v| v.as_str()).unwrap_or(device_id);
    if token.is_empty() || device_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"缺少 token 或 device_id"}))).into_response();
    }
    let Ok(claims) = middleware::parse_jwt(token, &s.config.jwt_secret) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"token 无效"}))).into_response();
    };
    let Some(user_id) = claims.get("user_id").and_then(|v| v.as_f64()) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"token 缺少 user_id"}))).into_response();
    };
    let h = s.hub.clone();
    let d = s.db.clone();
    let uid = user_id as u32;
    let did = device_id.to_string();
    let dn = device_name.to_string();
    ws.on_upgrade(move |socket| ws_hub::handle_websocket(socket, uid, did, dn, h, d)).into_response()
}

async fn serve_admin() -> impl IntoResponse {
    let html = include_str!("../static/index.html");
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

async fn fallback_setup() -> impl IntoResponse {
    let html = include_str!("../static/setup.html");
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"),(header::CACHE_CONTROL,"no-store, no-cache")], html)
}

async fn fallback_404() -> Json<Value> { Json(json!({"error":"Not Found"})) }

// ---- DB helpers ----

async fn open_db_simple(cfg: &Config) -> sqlx::SqlitePool {
    SqlitePoolOptions::new().max_connections(5).connect_lazy(&cfg.database_url()).unwrap()
}

/// 扫描本地存储目录，删除未被数据库引用的孤儿文件。
/// 仅清理 music/ 与 avatars/ 前缀；软删除歌曲的文件保留（可恢复）。
async fn cleanup_orphan_files(db: &sqlx::SqlitePool, local_path: &str) {
    let mut referenced = std::collections::HashSet::new();
    let oss_keys: Vec<(String,)> = sqlx::query_as("SELECT oss_key FROM musics WHERE oss_key != ''").fetch_all(db).await.unwrap_or_default();
    for (k,) in oss_keys { referenced.insert(k); }
    let cover_keys: Vec<(String,)> = sqlx::query_as("SELECT cover_key FROM musics WHERE cover_key != ''").fetch_all(db).await.unwrap_or_default();
    for (k,) in cover_keys { referenced.insert(k); }
    let lyrics_keys: Vec<(String,)> = sqlx::query_as("SELECT lyrics_key FROM musics WHERE lyrics_key != ''").fetch_all(db).await.unwrap_or_default();
    for (k,) in lyrics_keys { referenced.insert(k); }
    let avatars: Vec<(String,)> = sqlx::query_as("SELECT avatar FROM users WHERE avatar != ''").fetch_all(db).await.unwrap_or_default();
    for (k,) in avatars { referenced.insert(k); }

    let base = match std::path::absolute(local_path) { Ok(p) => p, Err(_) => return };
    let mut removed = 0usize;
    let mut stack = vec![base.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() { stack.push(path); continue; }
            let Ok(rel) = path.strip_prefix(&base) else { continue };
            let key = rel.to_string_lossy().replace('\\', "/");
            // 只清理受管的子目录，不碰 uploads 根目录下的其他文件
            if !(key.starts_with("music/") || key.starts_with("avatars/")) { continue; }
            if referenced.contains(&key) { continue; }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
                log::warn!("已清理孤儿文件: {}", key);
            }
        }
    }
    if removed > 0 {
        log::warn!("孤儿文件清理完成，共删除 {} 个", removed);
    } else {
        log::info!("存储检查完成，无孤儿文件");
    }
}

async fn init_database(cfg: &Config) -> Result<sqlx::SqlitePool, Box<dyn std::error::Error>> {
    if let Some(parent) = PathBuf::from(&cfg.db_path).parent() {
        if parent != std::path::Path::new(".") { std::fs::create_dir_all(parent)?; }
    }
    let pool = SqlitePoolOptions::new().max_connections(10).connect(&cfg.database_url()).await?;
    sqlx::query("PRAGMA journal_mode=WAL").execute(&pool).await?;
    sqlx::query("PRAGMA foreign_keys=ON").execute(&pool).await?;
    log::info!("SQLite数据库连接成功: {}", cfg.db_path);
    Ok(pool)
}

async fn run_migrations(db: &sqlx::SqlitePool) -> Result<(), Box<dyn std::error::Error>> {
    for sql in MIGRATIONS {
        sqlx::query(sql).execute(db).await?;
    }

    // 修复旧数据库中 devices.device_id 的错误 UNIQUE 约束
    // 旧 schema: device_id VARCHAR(64) NOT NULL UNIQUE (全局唯一)
    // 新 schema: UNIQUE(device_id, user_id) (每个用户内唯一)
    let create_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='devices'"
    ).fetch_one(db).await.unwrap_or_default();

    let needs_migration = create_sql.contains("device_id VARCHAR(64) NOT NULL UNIQUE");

    if needs_migration {
        log::warn!("检测到旧版 devices 表约束 (全局 UNIQUE)，正在迁移到 UNIQUE(device_id, user_id)...");
        // SQLite 不支持 ALTER TABLE DROP CONSTRAINT，需要重建表
        sqlx::query("DROP TABLE IF EXISTS devices_new").execute(db).await?;
        sqlx::query("CREATE TABLE devices_new (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, user_id INTEGER NOT NULL, device_id VARCHAR(64) NOT NULL, device_name VARCHAR(200) DEFAULT '', device_type VARCHAR(50) DEFAULT 'android', ip_address VARCHAR(45) DEFAULT '', is_online BOOLEAN DEFAULT 0, role VARCHAR(10) DEFAULT 'slave', sync_enabled BOOLEAN DEFAULT 1, last_seen DATETIME NOT NULL, UNIQUE(device_id, user_id))").execute(db).await?;
        sqlx::query("INSERT INTO devices_new SELECT * FROM devices").execute(db).await?;
        sqlx::query("DROP TABLE devices").execute(db).await?;
        sqlx::query("ALTER TABLE devices_new RENAME TO devices").execute(db).await?;
        log::info!("devices 表迁移完成");
    }

    log::info!("数据库迁移完成");
    Ok(())
}

const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS artists (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, name VARCHAR(255) NOT NULL UNIQUE, description VARCHAR(1000) DEFAULT '', avatar_url VARCHAR(1000) DEFAULT '')",
    "CREATE TABLE IF NOT EXISTS musics (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, title VARCHAR(255) NOT NULL, album VARCHAR(255) DEFAULT '', genre VARCHAR(100) DEFAULT '', lyrics TEXT DEFAULT '', duration REAL DEFAULT 0, size INTEGER DEFAULT 0, bitrate INTEGER DEFAULT 0, sample_rate INTEGER DEFAULT 0, channels INTEGER DEFAULT 0, format VARCHAR(10) DEFAULT '', codec VARCHAR(50) DEFAULT '', channel_count INTEGER DEFAULT 0, sort_order INTEGER DEFAULT 0, oss_key VARCHAR(500) NOT NULL DEFAULT '', lyrics_key VARCHAR(500) DEFAULT '', cover_key VARCHAR(500) DEFAULT '', fingerprint TEXT DEFAULT '', md5 VARCHAR(32) DEFAULT '')",
    "CREATE TABLE IF NOT EXISTS music_artists (id INTEGER PRIMARY KEY AUTOINCREMENT, music_id INTEGER NOT NULL, artist_id INTEGER NOT NULL, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS playlists (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, name VARCHAR(255) NOT NULL, description VARCHAR(1000) DEFAULT '', cover_url VARCHAR(1000) DEFAULT '', user_id INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS playlist_musics (id INTEGER PRIMARY KEY AUTOINCREMENT, playlist_id INTEGER NOT NULL, music_id INTEGER NOT NULL, added_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, sort_order INTEGER DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, username VARCHAR(100) NOT NULL UNIQUE, password VARCHAR(255) NOT NULL, email VARCHAR(255) DEFAULT '', avatar VARCHAR(1000) DEFAULT '')",
    "CREATE TABLE IF NOT EXISTS shares (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, music_id INTEGER NOT NULL, token VARCHAR(64) NOT NULL UNIQUE, expires_at DATETIME NOT NULL)",
    "CREATE TABLE IF NOT EXISTS devices (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at DATETIME, user_id INTEGER NOT NULL, device_id VARCHAR(64) NOT NULL, device_name VARCHAR(200) DEFAULT '', device_type VARCHAR(50) DEFAULT 'android', ip_address VARCHAR(45) DEFAULT '', is_online BOOLEAN DEFAULT 0, role VARCHAR(10) DEFAULT 'slave', sync_enabled BOOLEAN DEFAULT 1, last_seen DATETIME NOT NULL, UNIQUE(device_id, user_id))",
    "CREATE INDEX IF NOT EXISTS idx_musics_md5 ON musics(md5)",
    "CREATE INDEX IF NOT EXISTS idx_music_artists_music ON music_artists(music_id)",
    "CREATE INDEX IF NOT EXISTS idx_music_artists_artist ON music_artists(artist_id)",
    "CREATE INDEX IF NOT EXISTS idx_users_username ON users(username)",
    "CREATE INDEX IF NOT EXISTS idx_shares_token ON shares(token)",
];

// ============================================================
// 共用工具函数
// ============================================================

/// 加载指定音乐的歌手列表
async fn load_artists_for_music(db: &sqlx::SqlitePool, music_id: u32) -> Vec<Artist> {
    sqlx::query_as::<_, Artist>("SELECT a.* FROM artists a INNER JOIN music_artists ma ON a.id=ma.artist_id WHERE ma.music_id=? AND a.deleted_at IS NULL")
        .bind(music_id).fetch_all(db).await.unwrap_or_default()
}

/// 为 Music 注入临时 URL；include_lyrics=false 时省略全量歌词文本（列表场景减小 payload）
fn enrich(m: &Music, artists: &[Artist], include_lyrics: bool) -> Value {
    json!({
        "id": m.id,"created_at": m.created_at,"updated_at": m.updated_at,
        "title": m.title,"album": m.album,"genre": m.genre,
        "lyrics": if include_lyrics { m.lyrics.as_str() } else { "" },
        "duration": m.duration,"size": m.size,"bitrate": m.bitrate,
        "sample_rate": m.sample_rate,"channels": m.channels,
        "format": m.format,"codec": m.codec,"channel_count": m.channel_count,
        "sort_order": m.sort_order,"oss_key": m.oss_key,
        "cover_key": m.cover_key,"lyrics_key": m.lyrics_key,
        "fingerprint": m.fingerprint,"md5": m.md5,
        "artists": artists,
        "stream_url": if m.oss_key.is_empty() {String::new()} else {format!("/api/v1/music/{}/stream", m.id)},
        "download_url": if m.oss_key.is_empty() {String::new()} else {format!("/api/v1/music/{}/proxy-download", m.id)},
        "cover_url": if m.cover_key.is_empty() {String::new()} else {format!("/api/v1/music/{}/cover", m.id)},
        "lyrics_url": if m.lyrics_key.is_empty() {String::new()} else {format!("/api/v1/music/{}/lyrics", m.id)},
    })
}

/// 解析歌手字符串，支持 / ; 、 , & feat. 等分隔符（所有分隔符依次应用）
fn split_artists(s: &str) -> Vec<String> {
    let mut parts = vec![s.to_string()];
    for sep in &["/", ";", "、", ",", "&", " feat. ", " feat ", " Feat. ", " Feat "] {
        parts = parts.iter()
            .flat_map(|p| p.split(sep).map(|x| x.trim().to_string()).collect::<Vec<_>>())
            .filter(|p| !p.is_empty())
            .collect();
    }
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// 根据 MIME 类型返回图片扩展名
fn image_mime_ext(mime: &str) -> &str {
    match mime { "image/jpeg"=>".jpg","image/png"=>".png","image/gif"=>".gif","image/webp"=>".webp", _=>".jpg" }
}

/// 从 HeaderMap 提取 Authorization Bearer token 并解析 user_id
fn auth_user_id(headers: &HeaderMap, secret: &str) -> Option<u32> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    let claims = middleware::parse_jwt(token, secret).ok()?;
    claims.get("user_id").and_then(|v| v.as_f64()).map(|v| v as u32)
}

/// 简单 URL 编码
fn url_encode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z'|b'a'..=b'z'|b'0'..=b'9'|b'-'|b'_'|b'.' => format!("{}", b as char),
        b' ' => "+".into(),
        _ => format!("%{:02X}", b),
    }).collect()
}
