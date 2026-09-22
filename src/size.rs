//! Target segment size parsing.

use std::str::FromStr;

use crate::error::{AppError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetSize {
    bytes: u64,
}

impl TargetSize {
    pub fn bytes(self) -> u64 {
        self.bytes
    }
}

impl FromStr for TargetSize {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Err(AppError::new("目标分片大小不能为空"));
        }

        let unit_start = value
            .char_indices()
            .find_map(|(index, character)| {
                (!character.is_ascii_digit() && character != '.').then_some(index)
            })
            .unwrap_or(value.len());
        let (number, unit) = value.split_at(unit_start);
        let multiplier = match unit.to_ascii_lowercase().as_str() {
            "b" => 1,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "kib" => 1_024,
            "mib" => 1_048_576,
            "gib" => 1_073_741_824,
            _ => {
                return Err(AppError::new(format!(
                    "无效大小单位 `{unit}`；支持 B、KB、MB、GB、KiB、MiB、GiB"
                )));
            }
        };
        let bytes = parse_decimal_bytes(number, multiplier, value)?;
        if bytes == 0 {
            return Err(AppError::new("目标分片大小必须大于 0 字节"));
        }
        Ok(Self { bytes })
    }
}

fn parse_decimal_bytes(number: &str, multiplier: u64, original: &str) -> Result<u64> {
    let mut parts = number.split('.');
    let whole_text = parts.next().unwrap_or_default();
    let fraction_text = parts.next();
    if parts.next().is_some()
        || whole_text.is_empty()
        || !whole_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::new(format!("目标大小 `{original}` 格式无效")));
    }

    let whole: u64 = whole_text
        .parse()
        .map_err(|_| AppError::new(format!("目标大小 `{original}` 过大")))?;
    let fraction = match fraction_text {
        None => 0,
        Some(text)
            if !text.is_empty()
                && text.len() <= 3
                && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let value: u64 = text
                .parse()
                .map_err(|_| AppError::new(format!("目标大小 `{original}` 格式无效")))?;
            let divisor = 10_u64.pow(text.len() as u32);
            value
                .checked_mul(multiplier)
                .and_then(|scaled| scaled.checked_div(divisor))
                .ok_or_else(|| AppError::new(format!("目标大小 `{original}` 过大")))?
        }
        Some(_) => {
            return Err(AppError::new(format!(
                "目标大小 `{original}` 最多支持 3 位小数"
            )));
        }
    };

    whole
        .checked_mul(multiplier)
        .and_then(|bytes| bytes.checked_add(fraction))
        .ok_or_else(|| AppError::new(format!("目标大小 `{original}` 过大")))
}
