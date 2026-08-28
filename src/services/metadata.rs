//! 音频元数据提取：通过 ffprobe/ffmpeg 命令行工具获取标签、技术参数和封面图片。
//! 对应 Go 的 metadata.go。

use serde::Deserialize;
use std::process::Command;

/// ffprobe JSON 输出的顶层结构
#[derive(Debug, Deserialize)]
struct FFprobeOutput {
    format: Option<FFprobeFormat>,
    streams: Option<Vec<FFprobeStream>>,
}

#[derive(Debug, Deserialize)]
struct FFprobeFormat {
    #[serde(default)]
    tags: serde_json::Value,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    bit_rate: String,
}

#[derive(Debug, Deserialize)]
struct FFprobeStream {
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    sample_rate: String,
    #[serde(default)]
    channels: i32,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    bit_rate: String,
}

pub struct MetadataExtractor;

impl MetadataExtractor {
    pub fn new() -> Self {
        MetadataExtractor
    }
}

#[derive(Default, Clone, Debug)]
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

impl MetadataExtractor {
    /// 从音频文件提取所有元数据
    pub fn extract(
        &self,
        file_path: &str,
        file_name: &str,
        file_size: i64,
    ) -> MusicMetadata {
        let ext = std::path::Path::new(file_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let mut metadata = MusicMetadata {
            title: file_name
                .strip_suffix(&format!(".{}", ext))
                .unwrap_or(file_name)
                .to_string(),
            format: ext.to_lowercase(),
            size: file_size,
            ..Default::default()
        };

        // 通过 ffprobe 获取标签和流信息
        let probe_data = match run_ffprobe(file_path) {
            Ok(d) => d,
            Err(_) => return metadata,
        };

        // 解析文本标签
        if let Some(ref format) = probe_data.format {
            let tags = &format.tags;
            if let Some(t) = tag_str(tags, "title", "") {
                if !t.is_empty() && t != "0" {
                    metadata.title = t.to_string();
                }
            }
            metadata.artist = tag_str(tags, "artist", "").unwrap_or("").to_string();
            metadata.album = tag_str(tags, "album", "").unwrap_or("").to_string();
            metadata.genre = tag_str(tags, "genre", "").unwrap_or("").to_string();
            metadata.year = tag_int(tags, "date");
            metadata.track = tag_int(tags, "track");

            // lyrics 或 lyrics-XXX 格式
            let lyrics = tag_str(tags, "lyrics", "").unwrap_or("");
            if lyrics.is_empty() {
                if let Some(obj) = tags.as_object() {
                    for (k, v) in obj {
                        if k.to_lowercase().starts_with("lyrics-") {
                            metadata.lyrics = v.as_str().unwrap_or_default().to_string();
                            break;
                        }
                    }
                }
            } else {
                metadata.lyrics = lyrics.to_string();
            }

            // 容器级别时长和比特率
            if metadata.duration == 0.0 {
                metadata.duration = format.duration.parse().unwrap_or(0.0);
            }
            if metadata.bitrate == 0 {
                let br: i32 = format.bit_rate.parse().unwrap_or(0);
                metadata.bitrate = br / 1000; // bit/s → kbps
            }
        }

        // 从第一个音频流提取编码和采样信息
        if let Some(ref streams) = probe_data.streams {
            if let Some(s) = streams.first() {
                metadata.codec = codec_display_name(&s.codec_name);
                metadata.sample_rate = s.sample_rate.parse().unwrap_or(0);
                metadata.channels = s.channels;

                if metadata.duration == 0.0 {
                    metadata.duration = s.duration.parse().unwrap_or(0.0);
                }
                if metadata.bitrate == 0 {
                    let br: i32 = s.bit_rate.parse().unwrap_or(0);
                    if br > 0 {
                        metadata.bitrate = br / 1000;
                    }
                }
            }
        }

        // 提取内嵌封面图
        if let Ok((pic, mime)) = extract_cover(file_path) {
            if !pic.is_empty() {
                metadata.cover_data = pic;
                metadata.cover_mime = mime;
            }
        }

        metadata
    }
}

/// 调用 ffprobe 以 JSON 格式获取音频信息
fn run_ffprobe(file_path: &str) -> Result<FFprobeOutput, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_format",
            "-show_streams",
            "-of", "json",
            file_path,
        ])
        .output()
        .map_err(|e| format!("ffprobe 执行失败: {}", e))?;

    if !output.status.success() {
        return Err(format!("ffprobe: {}", String::from_utf8_lossy(&output.stderr)));
    }

    serde_json::from_slice(&output.stdout).map_err(|e| format!("解析 ffprobe JSON 失败: {}", e))
}

/// 通过 ffmpeg 提取内嵌封面图（输出到 image2pipe）
fn extract_cover(file_path: &str) -> Result<(Vec<u8>, String), String> {
    let output = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-i",
            file_path,
            "-an",
            "-c:v",
            "copy",
            "-f",
            "image2pipe",
            "-",
        ])
        .output()
        .map_err(|e| format!("ffmpeg 执行失败: {}", e))?;

    if !output.status.success() || output.stdout.len() < 64 {
        return Err("no cover".into());
    }

    let mime = detect_image_mime(&output.stdout);
    Ok((output.stdout.clone(), mime.to_string()))
}

/// 通过文件幻数识别图片 MIME 类型
fn detect_image_mime(data: &[u8]) -> &str {
    if data.len() < 4 {
        return "image/jpeg";
    }
    match (data[0], data[1], data.get(2).copied(), data.get(3).copied()) {
        (0xFF, 0xD8, _, _) => "image/jpeg",
        (0x89, b'P', Some(b'N'), Some(b'G')) => "image/png",
        (b'G', b'I', Some(b'F'), _) => "image/gif",
        (b'R', b'I', Some(b'F'), Some(b'F')) => "image/webp",
        _ => "image/jpeg",
    }
}

/// 从 ffprobe 标签 map 中提取字符串值（大小写不敏感）
fn tag_str<'a>(tags: &'a serde_json::Value, key: &str, fallback: &'a str) -> Option<&'a str> {
    let lower = key.to_lowercase();
    if let Some(obj) = tags.as_object() {
        for (k, v) in obj {
            if k.to_lowercase() == lower {
                if let Some(s) = v.as_str() {
                    return Some(s);
                }
            }
        }
    }
    if fallback.is_empty() {
        None
    } else {
        Some(fallback)
    }
}

/// 从 ffprobe 标签 map 中提取整数值（大小写不敏感）
fn tag_int(tags: &serde_json::Value, key: &str) -> i32 {
    let lower = key.to_lowercase();
    if let Some(obj) = tags.as_object() {
        for (k, v) in obj {
            if k.to_lowercase() == lower {
                if let Some(s) = v.as_str() {
                    let s = s.trim();
                    // 处理 "1/10" 格式
                    let num_part = s.split('/').next().unwrap_or(s);
                    return num_part.parse().unwrap_or(0);
                }
            }
        }
    }
    0
}

/// 编码器短名转人类可读名称
fn codec_display_name(codec: &str) -> String {
    match codec.to_lowercase().as_str() {
        "mp3" | "mp3float" => "MPEG Audio Layer 3".into(),
        "flac" => "FLAC".into(),
        "pcm_s16le" | "pcm_s16be" | "pcm" => "PCM".into(),
        "aac" | "aac_latm" => "AAC".into(),
        "vorbis" => "Vorbis".into(),
        "opus" => "Opus".into(),
        "ape" => "Monkey's Audio".into(),
        "wmav1" | "wmav2" | "wmapro" => "WMA".into(),
        _ => codec.to_uppercase(),
    }
}
