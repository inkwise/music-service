//! 中间件：CORS 跨域处理和 JWT 认证。
//! 对应 Go 的 middleware/cors.go 和 middleware/auth.go。

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// CORS 中间件（宽松模式，允许所有来源）
pub async fn cors_middleware(request: Request, next: Next) -> Response {
    // OPTIONS 预检请求直接返回 204
    if request.method() == axum::http::Method::OPTIONS {
        let mut response = Response::default();
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
        headers.insert(
            "Access-Control-Allow-Methods",
            "GET, POST, PUT, DELETE, OPTIONS".parse().unwrap(),
        );
        headers.insert(
            "Access-Control-Allow-Headers",
            "Origin, Content-Type, Authorization".parse().unwrap(),
        );
        headers.insert("Access-Control-Max-Age", "86400".parse().unwrap());
        return response;
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
    headers.insert(
        "Access-Control-Allow-Methods",
        "GET, POST, PUT, DELETE, OPTIONS".parse().unwrap(),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        "Origin, Content-Type, Authorization".parse().unwrap(),
    );
    headers.insert("Access-Control-Max-Age", "86400".parse().unwrap());
    response
}

/// JWT 认证中间件：从 Authorization header 提取并验证 Bearer token
pub async fn auth_middleware(
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, Response> {
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if auth_header.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "缺少认证令牌"})),
        )
            .into_response());
    }

    let parts: Vec<&str> = auth_header.splitn(2, ' ').collect();
    if parts.len() != 2 || parts[0] != "Bearer" {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "认证令牌格式无效"})),
        )
            .into_response());
    }

    // 从 Extension 获取 JWT secret
    let jwt_secret = request
        .extensions()
        .get::<String>()
        .cloned()
        .unwrap_or_default();

    match parse_jwt(parts[1], &jwt_secret) {
        Ok(claims) => {
            if let Some(user_id) = claims.get("user_id").and_then(|v| v.as_f64()) {
                request.extensions_mut().insert(user_id as u32);
                Ok(next.run(request).await)
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "令牌中缺少用户ID"})),
                )
                    .into_response())
            }
        }
        Err(_) => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "认证令牌无效或已过期"})),
        )
            .into_response()),
    }
}

/// 解析 JWT token，返回 claims
pub fn parse_jwt(token: &str, secret: &str) -> Result<serde_json::Value, String> {
    use jsonwebtoken::{decode, DecodingKey, Validation};

    let mut validation = Validation::default();
    validation.validate_exp = true;

    let token_data = decode::<serde_json::Value>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|e| format!("JWT 解析失败: {}", e))?;

    Ok(token_data.claims)
}

/// 生成 JWT token
pub fn generate_jwt(user_id: u32, secret: &str) -> Result<String, String> {
    use jsonwebtoken::{encode, EncodingKey, Header};
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize;
    let exp = now + 7 * 24 * 3600; // 7 天过期

    let claims = json!({
        "user_id": user_id,
        "exp": exp,
        "iat": now,
    });

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| format!("JWT 生成失败: {}", e))
}

/// 从 Request extensions 中提取 user_id
pub fn extract_user_id(request: &axum::extract::Request) -> Option<u32> {
    request.extensions().get::<u32>().copied()
}
