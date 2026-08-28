//! 音频指纹服务：Base64 编码/解码、Hamming 距离/相似度计算、fpcalc CLI 调用。
//! 对应 Go 的 fingerprint.go + fingerprint_fallback.go。

use base64::Engine;
use std::process::Command;
use tokio::sync::Semaphore;

pub struct FingerprintService {
    sem: Semaphore,
}

impl FingerprintService {
    pub fn new(max_workers: usize) -> Self {
        let workers = if max_workers == 0 { 2 } else { max_workers };
        FingerprintService {
            sem: Semaphore::new(workers),
        }
    }

    /// 检查指纹生成是否可用（fpcalc 是否存在于 PATH）
    pub fn available(&self) -> bool {
        fpcalc_available()
    }

    /// 生成音频指纹，返回 (base64指纹, 时长)
    pub async fn generate(&self, file_path: &str) -> Result<(String, f64), String> {
        let _permit = self
            .sem
            .acquire()
            .await
            .map_err(|e| format!("获取信号量失败: {}", e))?;

        fpcalc_generate(file_path)
    }
}

/// 检查 fpcalc 是否可用
fn fpcalc_available() -> bool {
    Command::new("fpcalc")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 调用 fpcalc 命令行工具生成指纹
fn fpcalc_generate(file_path: &str) -> Result<(String, f64), String> {
    let output = Command::new("fpcalc")
        .arg("-raw")
        .arg("-json")
        .arg(file_path)
        .output()
        .map_err(|e| format!("fpcalc 执行失败: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("fpcalc 返回错误: {}", stderr));
    }

    // fpcalc -json 输出格式：{"duration": 123.45, "fingerprint": [1,2,3,...]}
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("解析 fpcalc JSON 失败: {}", e))?;

    let duration = parsed["duration"]
        .as_f64()
        .ok_or("fpcalc 缺少 duration 字段")?;

    let fp_array = parsed["fingerprint"]
        .as_array()
        .ok_or("fpcalc 缺少 fingerprint 字段")?;

    let fp: Vec<u32> = fp_array
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect();

    if fp.is_empty() {
        return Err("fpcalc 返回空指纹".into());
    }

    let encoded = encode_fingerprint(&fp);
    Ok((encoded, duration))
}

// ============================================================
// 指纹编解码
// ============================================================

pub fn encode_fingerprint(fp: &[u32]) -> String {
    let mut buf = Vec::with_capacity(fp.len() * 4);
    for &v in fp {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(&buf)
}

pub fn decode_fingerprint(encoded: &str) -> Result<Vec<u32>, String> {
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Err("empty fingerprint".into());
    }

    // 包含逗号则为旧版格式：逗号分隔的整数
    if encoded.contains(',') {
        return parse_raw_fingerprint(encoded);
    }

    // Base64 解码
    let buf = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("invalid base64 fingerprint: {}", e))?;

    if buf.len() % 4 != 0 {
        return Err(format!(
            "fingerprint byte length {} is not a multiple of 4",
            buf.len()
        ));
    }

    let mut items = Vec::with_capacity(buf.len() / 4);
    for chunk in buf.chunks_exact(4) {
        let val = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        items.push(val);
    }
    Ok(items)
}

fn parse_raw_fingerprint(raw: &str) -> Result<Vec<u32>, String> {
    let mut items = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let val: u32 = part
            .parse()
            .map_err(|e| format!("invalid fingerprint value {:?}: {}", part, e))?;
        items.push(val);
    }
    if items.is_empty() {
        return Err("empty fingerprint".into());
    }
    Ok(items)
}

// ============================================================
// 指纹比对
// ============================================================

/// 计算两个指纹间的 Hamming 距离（不同比特位数）
pub fn hamming_distance(a: &[u32], b: &[u32]) -> usize {
    let min_len = a.len().min(b.len());
    let _max_len = a.len().max(b.len());

    let mut errors = 0usize;
    // 共同部分：异或后 count_ones
    for i in 0..min_len {
        errors += (a[i] ^ b[i]).count_ones() as usize;
    }
    // 长度差异部分：多余的 uint32 所有 32 位都计为不匹配
    if a.len() > min_len {
        for i in min_len..a.len() {
            errors += a[i].count_ones() as usize;
        }
    } else if b.len() > min_len {
        for i in min_len..b.len() {
            errors += b[i].count_ones() as usize;
        }
    }
    errors
}

/// 计算相似度 (0-1)，1 表示完全相同
pub fn similarity(a: &[u32], b: &[u32]) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 0.0;
    }
    let total_bits = max_len * 32;
    1.0 - (hamming_distance(a, b) as f64) / (total_bits as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let orig = vec![3162432899u32, 2494526144, 3131846657];
        let enc = encode_fingerprint(&orig);
        let dec = decode_fingerprint(&enc).unwrap();
        assert_eq!(dec, orig);
    }

    #[test]
    fn test_decode_legacy_comma() {
        let raw = "3162432899,2494526144,3131846657";
        let items = decode_fingerprint(raw).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], 3162432899);
    }

    #[test]
    fn test_decode_empty() {
        assert!(decode_fingerprint("").is_err());
    }

    #[test]
    fn test_hamming_identical() {
        let a = vec![0xAAAAAAAA, 0x55555555];
        let b = vec![0xAAAAAAAA, 0x55555555];
        assert_eq!(hamming_distance(&a, &b), 0);
    }

    #[test]
    fn test_hamming_complement() {
        let a = vec![0xFFFFFFFF];
        let b = vec![0x00000000];
        assert_eq!(hamming_distance(&a, &b), 32);
    }

    #[test]
    fn test_similarity_identical() {
        let a = vec![1, 2, 3];
        let b = vec![1, 2, 3];
        assert_eq!(similarity(&a, &b), 1.0);
    }

    #[test]
    fn test_similarity_complement() {
        let a = vec![0xFFFF0000];
        let b = vec![0x0000FFFF];
        assert_eq!(similarity(&a, &b), 0.0);
    }
}
