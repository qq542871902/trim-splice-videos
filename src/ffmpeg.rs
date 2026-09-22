//! FFmpeg and ffprobe process adapters.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{AppError, Result};

#[derive(Clone, Copy, Debug)]
pub struct KeyframeBoundary {
    pub elapsed_micros: u64,
    pub payload_offset: u128,
}

#[derive(Debug)]
pub struct PacketLayout {
    pub total_payload_bytes: u128,
    pub keyframes: Vec<KeyframeBoundary>,
}

pub struct Ffmpeg {
    binary: OsString,
    probe_binary: OsString,
}

impl Ffmpeg {
    pub fn from_environment() -> Self {
        Self {
            binary: env::var_os("FFMPEG_BIN").unwrap_or_else(|| OsString::from("ffmpeg")),
            probe_binary: env::var_os("FFPROBE_BIN").unwrap_or_else(|| OsString::from("ffprobe")),
        }
    }

    pub fn ensure_available(&self) -> Result<()> {
        ensure_program(&self.binary, "FFmpeg", "FFMPEG_BIN")
    }

    pub fn run(&self, args: &[OsString]) -> Result<()> {
        eprintln!("正在执行: {}", display_command(&self.binary, args));
        let status = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .map_err(|error| {
                AppError::new(format!(
                    "启动 `{}` 失败：{error}",
                    self.binary.to_string_lossy()
                ))
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(AppError::new(format!(
                "FFmpeg 执行失败（退出状态：{status}）"
            )))
        }
    }

    pub fn probe_packet_layout(&self, input: &Path) -> Result<PacketLayout> {
        ensure_program(&self.probe_binary, "ffprobe", "FFPROBE_BIN")?;
        let reference_stream = self.probe_reference_video_stream(input)?;
        eprintln!("正在分析媒体包大小与关键帧...");

        let mut child = Command::new(&self.probe_binary)
            .args([
                OsStr::new("-v"),
                OsStr::new("error"),
                OsStr::new("-show_packets"),
                OsStr::new("-show_entries"),
                OsStr::new("packet=stream_index,pts_time,size,flags"),
                OsStr::new("-of"),
                OsStr::new("compact=p=0:nk=0"),
            ])
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                AppError::new(format!(
                    "启动 `{}` 失败：{error}",
                    self.probe_binary.to_string_lossy()
                ))
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("无法读取 ffprobe 输出"))?;
        let parse_result = read_packet_layout(BufReader::new(stdout), reference_stream);
        if parse_result.is_err() {
            let _ = child.kill();
        }
        let status = child
            .wait()
            .map_err(|error| AppError::new(format!("等待 ffprobe 结束失败：{error}")))?;
        if !status.success() && parse_result.is_ok() {
            return Err(AppError::new(format!(
                "ffprobe 分析失败（退出状态：{status}）"
            )));
        }
        parse_result
    }

    fn probe_reference_video_stream(&self, input: &Path) -> Result<u32> {
        let output = Command::new(&self.probe_binary)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=index",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(input)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                AppError::new(format!(
                    "启动 `{}` 失败：{error}",
                    self.probe_binary.to_string_lossy()
                ))
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::new(format!(
                "ffprobe 无法读取视频流（退出状态：{}）：{}",
                output.status,
                detail.trim()
            )));
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|_| AppError::new("ffprobe 返回了非 UTF-8 的视频流信息"))?;
        value
            .lines()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| AppError::new("按大小分片需要至少一个视频流"))?
            .trim()
            .parse()
            .map_err(|_| AppError::new("ffprobe 返回了无效的视频流编号"))
    }
}

fn ensure_program(binary: &OsStr, label: &str, environment_variable: &str) -> Result<()> {
    let status = Command::new(binary)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| {
            AppError::new(format!(
                "无法运行 `{}`：{error}。请安装 {label} 或设置 {environment_variable}",
                binary.to_string_lossy()
            ))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "`{}` 不可用（退出状态：{status}）",
            binary.to_string_lossy()
        )))
    }
}

fn read_packet_layout(reader: impl BufRead, reference_stream: u32) -> Result<PacketLayout> {
    let mut total_payload_bytes = 0_u128;
    let mut keyframes = Vec::new();
    let mut first_keyframe_pts = None;
    let mut packet_number = 0_u64;

    for line in reader.lines() {
        packet_number += 1;
        let line = line.map_err(|error| {
            AppError::new(format!(
                "读取 ffprobe 第 {packet_number} 个媒体包失败：{error}"
            ))
        })?;
        if line.is_empty() {
            continue;
        }
        let packet = parse_packet(&line, packet_number)?;
        if packet.stream_index == reference_stream
            && packet.flags.contains('K')
            && let Some(pts) = packet.pts_micros
        {
            match first_keyframe_pts {
                None => first_keyframe_pts = Some(pts),
                Some(first_pts) => {
                    if let Ok(elapsed) = u64::try_from(pts.saturating_sub(first_pts)) {
                        let is_later =
                            keyframes.last().is_none_or(|boundary: &KeyframeBoundary| {
                                elapsed > boundary.elapsed_micros
                            });
                        if elapsed > 0 && is_later {
                            keyframes.push(KeyframeBoundary {
                                elapsed_micros: elapsed,
                                payload_offset: total_payload_bytes,
                            });
                        }
                    }
                }
            }
        }
        total_payload_bytes = total_payload_bytes
            .checked_add(u128::from(packet.size))
            .ok_or_else(|| AppError::new("媒体包总大小溢出"))?;
    }

    if packet_number == 0 || total_payload_bytes == 0 {
        return Err(AppError::new("ffprobe 未返回可用的媒体包"));
    }
    if first_keyframe_pts.is_none() {
        return Err(AppError::new("第一视频流中没有带时间戳的关键帧"));
    }
    Ok(PacketLayout {
        total_payload_bytes,
        keyframes,
    })
}

struct ProbePacket {
    stream_index: u32,
    pts_micros: Option<i64>,
    size: u64,
    flags: String,
}

fn parse_packet(line: &str, packet_number: u64) -> Result<ProbePacket> {
    let mut stream_index = None;
    let mut pts_micros = None;
    let mut size = None;
    let mut flags = String::new();

    for field in line.split('|') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "stream_index" => {
                stream_index = Some(value.parse().map_err(|_| {
                    AppError::new(format!("第 {packet_number} 个媒体包的流编号无效"))
                })?);
            }
            "pts_time" if value != "N/A" => {
                pts_micros = Some(parse_probe_time_micros(value, packet_number)?);
            }
            "size" => {
                size = Some(value.parse().map_err(|_| {
                    AppError::new(format!("第 {packet_number} 个媒体包的大小无效"))
                })?);
            }
            "flags" => flags = value.to_owned(),
            _ => {}
        }
    }

    Ok(ProbePacket {
        stream_index: stream_index
            .ok_or_else(|| AppError::new(format!("第 {packet_number} 个媒体包缺少流编号")))?,
        pts_micros,
        size: size.ok_or_else(|| AppError::new(format!("第 {packet_number} 个媒体包缺少大小")))?,
        flags,
    })
}

fn parse_probe_time_micros(value: &str, packet_number: u64) -> Result<i64> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::new(format!(
            "第 {packet_number} 个媒体包的时间戳 `{value}` 无效"
        )));
    }
    let whole: i128 = whole.parse().map_err(|_| {
        AppError::new(format!(
            "第 {packet_number} 个媒体包的时间戳 `{value}` 过大"
        ))
    })?;
    let fraction_value: i128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i128>().map_err(|_| {
            AppError::new(format!(
                "第 {packet_number} 个媒体包的时间戳 `{value}` 无效"
            ))
        })? * 10_i128.pow((6 - fraction.len()) as u32)
    };
    let micros = whole
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(fraction_value))
        .and_then(|value| {
            if negative {
                value.checked_neg()
            } else {
                Some(value)
            }
        })
        .ok_or_else(|| {
            AppError::new(format!(
                "第 {packet_number} 个媒体包的时间戳 `{value}` 过大"
            ))
        })?;
    i64::try_from(micros).map_err(|_| {
        AppError::new(format!(
            "第 {packet_number} 个媒体包的时间戳 `{value}` 过大"
        ))
    })
}

fn display_command(binary: &OsStr, args: &[OsString]) -> String {
    std::iter::once(binary)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|part| shell_quote(&part.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:=+,".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
