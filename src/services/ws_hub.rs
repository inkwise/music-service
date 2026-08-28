//! WebSocket Hub：以用户为单位管理设备连接，支持实时在线状态、心跳保活、
//! 主机控制从机同步播放。每个 WebSocket 连接对应一台设备。

use axum::extract::ws::{Message, WebSocket};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};

/// WebSocket 客户端元数据（在线期间存内存）
#[derive(Debug, Clone)]
pub struct ClientData {
    pub user_id: u32,
    pub device_id: String,
    pub device_name: String,
    pub role: String,
    pub sync_enabled: bool,
}

/// Hub 内部状态
struct HubInner {
    /// user_id → set of device_id
    users: HashMap<u32, HashSet<String>>,
    /// device_id → 发送通道（用于向该设备推送消息）
    clients: HashMap<String, mpsc::Sender<Message>>,
    /// device_id → 客户端元数据
    client_data: HashMap<String, ClientData>,
    /// device_id → 最近一次收到 pong 的时间
    last_pong: HashMap<String, Instant>,
    db: SqlitePool,
}

pub struct Hub {
    inner: RwLock<HubInner>,
}

/// 心跳间隔
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// pong 超时（超过此时间未收到 pong 视为断线）
const PONG_TIMEOUT: Duration = Duration::from_secs(12);

impl Hub {
    pub fn new(db: SqlitePool) -> Arc<Self> {
        let hub = Arc::new(Hub {
            inner: RwLock::new(HubInner {
                users: HashMap::new(),
                clients: HashMap::new(),
                client_data: HashMap::new(),
                last_pong: HashMap::new(),
                db,
            }),
        });

        // 启动心跳检测后台任务
        let h = hub.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PING_INTERVAL);
            loop {
                ticker.tick().await;
                h.heartbeat_tick().await;
            }
        });

        hub
    }

    // ============================================================
    // 注册 / 注销
    // ============================================================

    /// 设备上线：WebSocket 连接建立后调用，更新内存 + DB 状态
    pub async fn register(
        &self,
        user_id: u32,
        device_id: &str,
        data: ClientData,
        tx: mpsc::Sender<Message>,
    ) {
        let mut inner = self.inner.write().await;
        let now_ts = Utc::now().naive_utc();

        // 内存
        inner.clients.insert(device_id.to_string(), tx);
        inner.client_data.insert(device_id.to_string(), data);
        inner.last_pong.insert(device_id.to_string(), Instant::now());
        inner
            .users
            .entry(user_id)
            .or_insert_with(HashSet::new)
            .insert(device_id.to_string());

        // DB: 标记在线
        let _ = sqlx::query(
            "UPDATE devices SET is_online=1, last_seen=?, updated_at=? WHERE device_id=?",
        )
        .bind(now_ts)
        .bind(now_ts)
        .bind(device_id)
        .execute(&inner.db)
        .await;

        // 通知同用户其他设备：有新设备上线
        let msg = serde_json::json!({
            "type": "device_online",
            "payload": {
                "device_id": device_id,
                "user_id": user_id,
                "is_online": true
            }
        });
        let msg_str = serde_json::to_string(&msg).unwrap();
        // 释放写锁再广播（广播内部需要读锁）
        drop(inner);
        self.broadcast_to_user_except(user_id, device_id, &msg_str).await;
    }

    /// 设备下线：WebSocket 断开后调用，更新内存 + DB 状态。
    /// 仅当注册的发送通道与当前连接一致时才清理：
    /// 同 device_id 重连后，旧连接退出时不能误删新连接的在线状态。
    pub async fn unregister_if_current(&self, device_id: &str, tx: &mpsc::Sender<Message>) {
        let mut inner = self.inner.write().await;

        // 该设备已被新连接重新注册（或本就不在线）→ 跳过清理
        let is_current = inner
            .clients
            .get(device_id)
            .map(|cur| cur.same_channel(tx))
            .unwrap_or(false);
        if !is_current {
            log::info!("设备 {} 的旧连接退出，忽略（已由新连接接管）", device_id);
            return;
        }

        let now_ts = Utc::now().naive_utc();
        let mut user_id_opt = None;
        let mut was_host = false;

        // 清理内存
        inner.clients.remove(device_id);
        inner.last_pong.remove(device_id);

        if let Some(data) = inner.client_data.remove(device_id) {
            user_id_opt = Some(data.user_id);
            was_host = data.role == "host";
            if let Some(devices) = inner.users.get_mut(&data.user_id) {
                devices.remove(device_id);
                if devices.is_empty() {
                    inner.users.remove(&data.user_id);
                }
            }
        }

        // DB: 标记离线
        let _ = sqlx::query(
            "UPDATE devices SET is_online=0, last_seen=?, updated_at=? WHERE device_id=?",
        )
        .bind(now_ts)
        .bind(now_ts)
        .bind(device_id)
        .execute(&inner.db)
        .await;

        let uid = user_id_opt.unwrap_or(0);
        drop(inner);

        // 通知其他设备
        if uid > 0 {
            let offline_msg = serde_json::json!({
                "type": "device_offline",
                "payload": {
                    "device_id": device_id,
                    "is_online": false
                }
            });
            let s = serde_json::to_string(&offline_msg).unwrap();
            self.broadcast_to_user_except(uid, device_id, &s).await;

            // 主机断线额外通知
            if was_host {
                let host_msg = serde_json::json!({
                    "type": "host_disconnected",
                    "payload": { "device_id": device_id }
                });
                let s2 = serde_json::to_string(&host_msg).unwrap();
                self.broadcast_to_user_except(uid, device_id, &s2).await;
            }
        }
    }

    // ============================================================
    // 查询
    // ============================================================

    pub async fn get_user_clients(&self, user_id: u32) -> Vec<ClientData> {
        let inner = self.inner.read().await;
        let mut result = Vec::new();
        if let Some(devices) = inner.users.get(&user_id) {
            for id in devices {
                if let Some(data) = inner.client_data.get(id) {
                    result.push(data.clone());
                }
            }
        }
        result
    }

    pub async fn is_online(&self, device_id: &str) -> bool {
        self.inner.read().await.clients.contains_key(device_id)
    }

    // ============================================================
    // 广播
    // ============================================================

    pub async fn broadcast_to_user(&self, user_id: u32, msg: &str) {
        let inner = self.inner.read().await;
        if let Some(devices) = inner.users.get(&user_id) {
            for id in devices {
                if let Some(tx) = inner.clients.get(id) {
                    let _ = tx.send(Message::Text(msg.to_string().into())).await;
                }
            }
        }
    }

    pub async fn broadcast_to_slaves(&self, user_id: u32, msg: &str) {
        let inner = self.inner.read().await;
        if let Some(devices) = inner.users.get(&user_id) {
            for id in devices {
                if let Some(data) = inner.client_data.get(id) {
                    if data.role == "slave" && data.sync_enabled {
                        if let Some(tx) = inner.clients.get(id) {
                            let _ = tx.send(Message::Text(msg.to_string().into())).await;
                        }
                    }
                }
            }
        }
    }

    pub async fn broadcast_to_user_except(&self, user_id: u32, except: &str, msg: &str) {
        let inner = self.inner.read().await;
        if let Some(devices) = inner.users.get(&user_id) {
            for id in devices {
                if id != except {
                    if let Some(tx) = inner.clients.get(id) {
                        let _ = tx.send(Message::Text(msg.to_string().into())).await;
                    }
                }
            }
        }
    }

    // ============================================================
    // 角色 / 同步控制
    // ============================================================

    /// 主机踢出从机：禁用该从机的同步功能
    pub async fn kick_slave(&self, device_id: &str) {
        let uid = {
            let mut inner = self.inner.write().await;
            let _ = sqlx::query(
                "UPDATE devices SET sync_enabled=0, updated_at=? WHERE device_id=?",
            )
            .bind(Utc::now().naive_utc())
            .bind(device_id)
            .execute(&inner.db)
            .await;
            if let Some(data) = inner.client_data.get_mut(device_id) {
                data.sync_enabled = false;
                data.user_id
            } else {
                0
            }
        };

        if uid > 0 {
            // 通知被踢从机
            let msg = serde_json::json!({
                "type": "slave_kicked",
                "payload": { "device_id": device_id }
            });
            let s = serde_json::to_string(&msg).unwrap();
            let inner = self.inner.read().await;
            if let Some(tx) = inner.clients.get(device_id) {
                let _ = tx.send(Message::Text(s.into())).await;
            }
            // 通知所有设备状态变更
            drop(inner);
            let status_msg = serde_json::json!({
                "type": "slave_sync_toggled",
                "payload": { "device_id": device_id, "enabled": false, "kicked": true }
            });
            let s2 = serde_json::to_string(&status_msg).unwrap();
            self.broadcast_to_user(uid, &s2).await;
        }
    }

    pub async fn set_role(&self, device_id: &str, role: &str) {
        let mut inner = self.inner.write().await;
        if let Some(data) = inner.client_data.get_mut(device_id) {
            let old_role = data.role.clone();
            data.role = role.to_string();
            let user_id = data.user_id;

            let _ = sqlx::query("UPDATE devices SET role=?, updated_at=? WHERE device_id=?")
                .bind(role)
                .bind(Utc::now().naive_utc())
                .bind(device_id)
                .execute(&inner.db)
                .await;

            // 升为主机时，同用户其他主机降级
            if role == "host" && old_role != "host" {
                // Clone device IDs to avoid borrow conflict
                let other_ids: Vec<String> = inner
                    .users
                    .get(&user_id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                for other_id in &other_ids {
                    if other_id != device_id {
                        if let Some(other) = inner.client_data.get_mut(other_id) {
                            if other.role == "host" {
                                other.role = "slave".to_string();
                            }
                        }
                    }
                }
                let _ = sqlx::query(
                    "UPDATE devices SET role='slave' WHERE user_id=? AND device_id!=? AND role='host'",
                )
                .bind(user_id)
                .bind(device_id)
                .execute(&inner.db)
                .await;
            }

            let msg = serde_json::json!({
                "type": "role_changed",
                "payload": { "device_id": device_id, "role": role }
            });
            let s = serde_json::to_string(&msg).unwrap();
            drop(inner);
            self.broadcast_to_user_except(user_id, device_id, &s).await;
        }
    }

    pub async fn set_slave_enabled(&self, device_id: &str, enabled: bool) {
        let inner = self.inner.read().await;
        let user_id = inner
            .client_data
            .get(device_id)
            .map(|d| d.user_id)
            .unwrap_or(0);
        drop(inner);

        {
            let mut inner = self.inner.write().await;
            if let Some(data) = inner.client_data.get_mut(device_id) {
                data.sync_enabled = enabled;
                let _ = sqlx::query(
                    "UPDATE devices SET sync_enabled=?, updated_at=? WHERE device_id=?",
                )
                .bind(enabled)
                .bind(Utc::now().naive_utc())
                .bind(device_id)
                .execute(&inner.db)
                .await;
            }
        }

        if user_id > 0 {
            let msg = serde_json::json!({
                "type": "slave_sync_toggled",
                "payload": { "device_id": device_id, "enabled": enabled }
            });
            let s = serde_json::to_string(&msg).unwrap();
            // 通知主机
            let clients = self.get_user_clients(user_id).await;
            for c in &clients {
                if c.role == "host" || c.device_id == device_id {
                    let inner = self.inner.read().await;
                    if let Some(tx) = inner.clients.get(&c.device_id) {
                        let _ = tx.send(Message::Text(s.clone().into())).await;
                    }
                }
            }
        }
    }

    // ============================================================
    // 心跳检测（后台任务，每 PING_INTERVAL 触发一次）
    // ============================================================

    async fn heartbeat_tick(&self) {
        let mut inner = self.inner.write().await;
        let now = Instant::now();
        let mut timed_out: Vec<String> = Vec::new();

        // 1. 给所有在线设备发送 ping
        for (_did, tx) in &inner.clients {
            let _ = tx.send(Message::Ping(vec![].into())).await;
        }

        // 2. 检查哪些设备 pong 超时
        for (did, last) in &inner.last_pong {
            if now.duration_since(*last) > PONG_TIMEOUT {
                timed_out.push(did.clone());
            }
        }

        // 3. 踢掉超时设备
        let db = inner.db.clone();
        let now_ts = Utc::now().naive_utc();
        for did in &timed_out {
            log::warn!("设备 {} 心跳超时，断开连接", did);
            inner.clients.remove(did);
            inner.last_pong.remove(did);

            let mut uid = 0u32;
            if let Some(data) = inner.client_data.remove(did) {
                uid = data.user_id;
                if let Some(devices) = inner.users.get_mut(&data.user_id) {
                    devices.remove(did);
                    if devices.is_empty() {
                        inner.users.remove(&data.user_id);
                    }
                }
            }

            // DB 标记离线
            let _ = sqlx::query(
                "UPDATE devices SET is_online=0, last_seen=?, updated_at=? WHERE device_id=?",
            )
            .bind(now_ts)
            .bind(now_ts)
            .bind(did)
            .execute(&db)
            .await;

            // 通知其他设备
            if uid > 0 {
                let msg = serde_json::json!({
                    "type": "device_offline",
                    "payload": { "device_id": did, "is_online": false, "reason": "heartbeat_timeout" }
                });
                let s = serde_json::to_string(&msg).unwrap();
                drop(inner);
                self.broadcast_to_user_except(uid, did, &s).await;
                inner = self.inner.write().await;
            }
        }
    }

    /// WebSocket Pong 回调：更新设备心跳时间
    pub async fn record_pong(&self, device_id: &str) {
        let mut inner = self.inner.write().await;
        inner
            .last_pong
            .insert(device_id.to_string(), Instant::now());
    }
}

// ============================================================
// WebSocket 连接处理
// ============================================================

pub async fn handle_websocket(
    socket: WebSocket,
    user_id: u32,
    device_id: String,
    device_name: String,
    hub: Arc<Hub>,
    db: SqlitePool,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(256);

    // 从 DB 恢复角色和同步状态
    let role: String = sqlx::query_scalar(
        "SELECT role FROM devices WHERE device_id=? AND user_id=?",
    )
    .bind(&device_id)
    .bind(user_id)
    .fetch_optional(&db)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "slave".to_string());

    let sync_enabled: bool = sqlx::query_scalar(
        "SELECT sync_enabled FROM devices WHERE device_id=? AND user_id=?",
    )
    .bind(&device_id)
    .bind(user_id)
    .fetch_optional(&db)
    .await
    .ok()
    .flatten()
    .unwrap_or(true);

    // 注册到 Hub（自动更新 DB is_online=true）
    hub.register(
        user_id,
        &device_id,
        ClientData {
            user_id,
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            role: role.clone(),
            sync_enabled,
        },
        tx.clone(),
    )
    .await;

    log::info!(
        "设备上线: user={} device={} role={}",
        user_id,
        device_id,
        role
    );

    // ---- Write pump: 推送消息到 WebSocket ----
    let write_hub = hub.clone();
    let write_dev = device_id.clone();
    let write_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
        // WebSocket 写入失败 = 连接断开
        write_hub.unregister_if_current(&write_dev, &write_tx).await;
        log::info!("设备 write pump 退出: {}", write_dev);
    });

    // ---- Read pump: 从 WebSocket 读取消息 ----
    let read_hub = hub.clone();
    let read_dev = device_id.clone();
    let read_user = user_id;

    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                let text_str: &str = &text;
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(text_str) {
                    let msg_type = obj["type"].as_str().unwrap_or("");
                    match msg_type {
                        // 客户端主动上报在线状态
                        "status" | "online" => {
                            let is_online = obj["is_online"]
                                .as_bool()
                                .unwrap_or(true);
                            if is_online {
                                read_hub.record_pong(&read_dev).await;
                            }
                        }

                        // 心跳 ping
                        "ping" => {
                            read_hub.record_pong(&read_dev).await;
                            let pong = serde_json::json!({"type":"pong","timestamp_ms": chrono::Utc::now().timestamp_millis()});
                            let s = serde_json::to_string(&pong).unwrap();
                            let _ = tx.send(Message::Text(s.into())).await;
                        }

                        // NTP 时间同步请求
                        "ntp_request" => {
                            read_hub.record_pong(&read_dev).await;
                            let now = chrono::Utc::now();
                            let t2 = now.timestamp_nanos_opt().unwrap_or(0);
                            let t3 = t2;
                            let t1 = obj["t1"].as_f64().unwrap_or(0.0) as i64;
                            let resp = serde_json::json!({
                                "type": "ntp_result",
                                "t1": t1, "t2": t2, "t3": t3
                            });
                            let s = serde_json::to_string(&resp).unwrap();
                            let _ = tx.send(Message::Text(s.into())).await;
                        }

                        // 切换角色 / 同步开关
                        "set_role" => {
                            read_hub.record_pong(&read_dev).await;
                            if let Some(r) = obj["role"].as_str() {
                                if r == "host" || r == "slave" {
                                    // 处理同步开关
                                    if let Some(enabled) = obj["sync_enabled"].as_bool() {
                                        read_hub.set_slave_enabled(&read_dev, enabled).await;
                                    }
                                    read_hub.set_role(&read_dev, r).await;
                                    // 广播新的设备列表状态给所有设备
                                    let status = serde_json::json!({
                                        "type": "role_changed",
                                        "payload": {
                                            "device_id": read_dev,
                                            "role": r,
                                            "user_id": read_user
                                        }
                                    });
                                    read_hub.broadcast_to_user(
                                        read_user,
                                        &serde_json::to_string(&status).unwrap(),
                                    ).await;
                                }
                            }
                        }

                        // 主机踢出从机
                        "kick_slave" => {
                            read_hub.record_pong(&read_dev).await;
                            if role == "host" {
                                if let Some(payload) = obj["payload"].as_object() {
                                    if let Some(target) = payload.get("device_id").and_then(|v| v.as_str()) {
                                        read_hub.kick_slave(target).await;
                                    }
                                }
                            }
                        }

                        // 切换从机同步开关
                        "toggle_slave" => {
                            read_hub.record_pong(&read_dev).await;
                            if let Some(payload) = obj["payload"].as_object() {
                                if let Some(slave_id) =
                                    payload.get("device_id").and_then(|v| v.as_str())
                                {
                                    let enabled = payload
                                        .get("enabled")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    read_hub.set_slave_enabled(slave_id, enabled).await;
                                }
                            }
                        }

                        // 主机指令转发给从机
                        _ => {
                            read_hub.record_pong(&read_dev).await;
                            if role == "host" {
                                let enriched = serde_json::json!({
                                    "type": msg_type,
                                    "sender_device_id": read_dev,
                                    "payload": obj["payload"],
                                    "timestamp_ms": obj["timestamp_ms"],
                                });
                                read_hub
                                    .broadcast_to_slaves(
                                        read_user,
                                        &serde_json::to_string(&enriched).unwrap(),
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }

            Ok(Message::Ping(_)) => {
                // axum 自动回复 pong，我们只需要记录心跳
                read_hub.record_pong(&read_dev).await;
            }

            Ok(Message::Pong(_)) => {
                read_hub.record_pong(&read_dev).await;
            }

            Ok(Message::Binary(_)) => {
                // 忽略二进制消息
            }

            Ok(Message::Close(_)) | Err(_) => {
                break;
            }
        }
    }

    // WebSocket 断开，注销设备（若已被同 id 新连接接管则跳过）
    read_hub.unregister_if_current(&read_dev, &tx).await;
    log::info!("设备下线: user={} device={}", read_user, read_dev);
}
