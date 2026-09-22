//! Video timestamp parsing and formatting.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use crate::error::{AppError, Result};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timestamp {
    millis: u64,
}

impl Timestamp {
    pub fn as_ffmpeg_seconds(self) -> String {
        format!("{}.{:03}", self.millis / 1_000, self.millis % 1_000)
    }
}

impl FromStr for Timestamp {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Err(AppError::new("时间点不能为空"));
        }

        let fields: Vec<&str> = value.split(':').collect();
        let millis = match fields.as_slice() {
            [seconds] => parse_seconds(seconds, false)?,
            [minutes, seconds] => {
                let minutes = parse_integer(minutes, "分钟")?;
                checked_millis(minutes, 60_000, parse_seconds(seconds, true)?)?
            }
            [hours, minutes, seconds] => {
                let hours = parse_integer(hours, "小时")?;
                let minutes = parse_integer(minutes, "分钟")?;
                if minutes >= 60 {
                    return Err(AppError::new(format!(
                        "时间点 `{value}` 中的分钟必须小于 60"
                    )));
                }
                let hour_millis = hours
                    .checked_mul(3_600_000)
                    .ok_or_else(|| AppError::new(format!("时间点 `{value}` 过大")))?;
                let minute_millis = minutes
                    .checked_mul(60_000)
                    .ok_or_else(|| AppError::new(format!("时间点 `{value}` 过大")))?;
                let second_millis = parse_seconds(seconds, true)?;
                hour_millis
                    .checked_add(minute_millis)
                    .and_then(|total| total.checked_add(second_millis))
                    .ok_or_else(|| AppError::new(format!("时间点 `{value}` 过大")))?
            }
            _ => {
                return Err(AppError::new(format!(
                    "无效时间点 `{value}`，请使用 SS、MM:SS 或 HH:MM:SS"
                )));
            }
        };

        if millis == 0 {
            return Err(AppError::new("拆分时间点必须大于 0"));
        }
        Ok(Self { millis })
    }
}

impl Display for Timestamp {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let total_seconds = self.millis / 1_000;
        let hours = total_seconds / 3_600;
        let minutes = (total_seconds % 3_600) / 60;
        let seconds = total_seconds % 60;
        write!(
            formatter,
            "{hours:02}:{minutes:02}:{seconds:02}.{:03}",
            self.millis % 1_000
        )
    }
}

fn parse_integer(value: &str, label: &str) -> Result<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AppError::new(format!("{label} `{value}` 不是有效整数")));
    }
    value
        .parse()
        .map_err(|_| AppError::new(format!("{label} `{value}` 过大")))
}

fn parse_seconds(value: &str, must_be_below_sixty: bool) -> Result<u64> {
    let mut parts = value.split('.');
    let whole_text = parts.next().unwrap_or_default();
    let fraction_text = parts.next();
    if parts.next().is_some() {
        return Err(AppError::new(format!("秒数 `{value}` 格式无效")));
    }

    let whole = parse_integer(whole_text, "秒")?;
    if must_be_below_sixty && whole >= 60 {
        return Err(AppError::new(format!("秒数 `{value}` 必须小于 60")));
    }

    let fraction = match fraction_text {
        None => 0,
        Some(text)
            if !text.is_empty()
                && text.len() <= 3
                && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let parsed: u64 = text
                .parse()
                .map_err(|_| AppError::new(format!("秒数 `{value}` 格式无效")))?;
            parsed * 10_u64.pow((3 - text.len()) as u32)
        }
        Some(_) => {
            return Err(AppError::new(format!("秒数 `{value}` 最多支持 3 位小数")));
        }
    };

    whole
        .checked_mul(1_000)
        .and_then(|millis| millis.checked_add(fraction))
        .ok_or_else(|| AppError::new(format!("秒数 `{value}` 过大")))
}

fn checked_millis(value: u64, multiplier: u64, tail: u64) -> Result<u64> {
    value
        .checked_mul(multiplier)
        .and_then(|millis| millis.checked_add(tail))
        .ok_or_else(|| AppError::new("时间点过大"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentDuration {
    millis: u64,
}

impl SegmentDuration {
    pub fn as_ffmpeg_seconds(self) -> String {
        format!("{}.{:03}", self.millis / 1_000, self.millis % 1_000)
    }
}

impl FromStr for SegmentDuration {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Err(AppError::new("分段时长不能为空"));
        }

        let millis = match value.char_indices().last() {
            Some((unit_index, unit)) if matches!(unit.to_ascii_lowercase(), 's' | 'm' | 'h') => {
                let multiplier = match unit.to_ascii_lowercase() {
                    's' => 1_000,
                    'm' => 60_000,
                    'h' => 3_600_000,
                    _ => unreachable!(),
                };
                parse_scaled_duration(&value[..unit_index], multiplier, value)?
            }
            _ => value
                .parse::<Timestamp>()
                .map(|timestamp| timestamp.millis)
                .map_err(|_| {
                    AppError::new(format!(
                        "无效分段时长 `{value}`；请使用 9m、540s、1h 或 00:09:00"
                    ))
                })?,
        };

        if millis == 0 {
            return Err(AppError::new("分段时长必须大于 0"));
        }
        Ok(Self { millis })
    }
}

fn parse_scaled_duration(number: &str, multiplier: u64, original: &str) -> Result<u64> {
    let mut parts = number.split('.');
    let whole_text = parts.next().unwrap_or_default();
    let fraction_text = parts.next();
    if parts.next().is_some() {
        return Err(AppError::new(format!("分段时长 `{original}` 格式无效")));
    }

    let whole = parse_integer(whole_text, "时长")?;
    let fraction = match fraction_text {
        None => 0,
        Some(text)
            if !text.is_empty()
                && text.len() <= 3
                && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let value: u64 = text
                .parse()
                .map_err(|_| AppError::new(format!("分段时长 `{original}` 格式无效")))?;
            let divisor = 10_u64.pow(text.len() as u32);
            value
                .checked_mul(multiplier)
                .and_then(|scaled| scaled.checked_div(divisor))
                .ok_or_else(|| AppError::new(format!("分段时长 `{original}` 过大")))?
        }
        Some(_) => {
            return Err(AppError::new(format!(
                "分段时长 `{original}` 最多支持 3 位小数"
            )));
        }
    };

    whole
        .checked_mul(multiplier)
        .and_then(|millis| millis.checked_add(fraction))
        .ok_or_else(|| AppError::new(format!("分段时长 `{original}` 过大")))
}
