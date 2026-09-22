//! Lossless split and concat application services.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::{ConcatArgs, SplitArgs, SplitMode};
use crate::error::{AppError, Result};
use crate::ffmpeg::{Ffmpeg, KeyframeBoundary, PacketLayout};

pub fn split(ffmpeg: &Ffmpeg, request: SplitArgs) -> Result<Vec<PathBuf>> {
    let SplitArgs {
        input,
        mode,
        output_dir,
        prefix,
        overwrite,
    } = request;
    require_input_file(&input)?;

    let extension = utf8_component(input.extension(), "输入视频缺少文件扩展名")?;
    let default_prefix = utf8_component(input.file_stem(), "无法确定输入视频文件名")?;
    let prefix = prefix.unwrap_or(default_prefix);
    let output_dir = output_dir.unwrap_or_else(|| {
        input
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{prefix}_parts"))
    });
    create_output_directory(&output_dir)?;
    reject_split_input_collision(&input, &output_dir, &prefix, &extension)?;
    let pattern = segment_pattern(&output_dir, &prefix, &extension)?;

    let (mode_args, expected_outputs) = match mode {
        SplitMode::At(times) => {
            ensure_strictly_increasing(&times)?;
            let outputs: Vec<PathBuf> = (1..=times.len() + 1)
                .map(|index| output_dir.join(format!("{prefix}-{index:03}.{extension}")))
                .collect();
            prepare_split_outputs(&output_dir, &outputs, overwrite)?;
            let segment_times = times
                .iter()
                .map(|time| time.as_ffmpeg_seconds())
                .collect::<Vec<_>>()
                .join(",");
            (
                vec!["-segment_times".into(), segment_times.into()],
                Some(outputs),
            )
        }
        SplitMode::Every(duration) => {
            prepare_periodic_outputs(&output_dir, &prefix, &extension, overwrite)?;
            (
                vec!["-segment_time".into(), duration.as_ffmpeg_seconds().into()],
                None,
            )
        }
        SplitMode::Size(target) => {
            return split_by_size(
                ffmpeg,
                &input,
                &output_dir,
                &prefix,
                &extension,
                target.bytes(),
                overwrite,
            );
        }
    };

    let mut args = vec![
        "-hide_banner".into(),
        overwrite_flag(overwrite).into(),
        "-i".into(),
        input.into_os_string(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-f".into(),
        "segment".into(),
    ];
    args.extend(mode_args);
    args.extend([
        "-reset_timestamps".into(),
        "1".into(),
        "-segment_start_number".into(),
        "1".into(),
        pattern.into_os_string(),
    ]);
    ffmpeg.run(&args)?;

    match expected_outputs {
        Some(outputs) => {
            let expected_count = outputs.len();
            let generated: Vec<PathBuf> =
                outputs.into_iter().filter(|path| path.is_file()).collect();
            if generated.len() != expected_count {
                return Err(AppError::new(format!(
                    "预期生成 {expected_count} 段，实际生成 {} 段；请检查拆分点是否超过视频时长。已生成文件保留在 `{}`",
                    generated.len(),
                    output_dir.display()
                )));
            }
            Ok(generated)
        }
        None => discover_periodic_outputs(&output_dir, &prefix, &extension),
    }
}

fn split_by_size(
    ffmpeg: &Ffmpeg,
    input: &Path,
    output_dir: &Path,
    prefix: &str,
    extension: &str,
    target_bytes: u64,
    overwrite: bool,
) -> Result<Vec<PathBuf>> {
    let input_size = input
        .metadata()
        .map_err(|error| AppError::new(format!("无法读取 `{}` 的大小：{error}", input.display())))?
        .len();
    if input_size == 0 {
        return Err(AppError::new("输入视频为空文件"));
    }
    let layout = ffmpeg.probe_packet_layout(input)?;
    let split_times = choose_size_split_times(&layout, target_bytes, input_size)?;
    prepare_periodic_outputs(output_dir, prefix, extension, overwrite)?;

    if split_times.is_empty() {
        let output = output_dir.join(format!("{prefix}-001.{extension}"));
        let args = vec![
            "-hide_banner".into(),
            overwrite_flag(overwrite).into(),
            "-i".into(),
            input.as_os_str().to_owned(),
            "-map".into(),
            "0".into(),
            "-c".into(),
            "copy".into(),
            output.as_os_str().to_owned(),
        ];
        ffmpeg.run(&args)?;
        if !output.is_file() {
            return Err(AppError::new(format!(
                "FFmpeg 未生成输出文件 `{}`",
                output.display()
            )));
        }
        return Ok(vec![output]);
    }

    let pattern = segment_pattern(output_dir, prefix, extension)?;
    let expected_count = split_times.len() + 1;
    let args = vec![
        "-hide_banner".into(),
        overwrite_flag(overwrite).into(),
        "-i".into(),
        input.as_os_str().to_owned(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-f".into(),
        "segment".into(),
        "-reference_stream".into(),
        "v:0".into(),
        "-segment_times".into(),
        split_times.join(",").into(),
        "-segment_time_delta".into(),
        "0.000001".into(),
        "-reset_timestamps".into(),
        "1".into(),
        "-segment_start_number".into(),
        "1".into(),
        pattern.into_os_string(),
    ];
    ffmpeg.run(&args)?;

    let outputs = discover_periodic_outputs(output_dir, prefix, extension)?;
    if outputs.len() != expected_count {
        return Err(AppError::new(format!(
            "按大小计算出 {expected_count} 段，但 FFmpeg 实际生成 {} 段；已生成文件保留在 `{}`",
            outputs.len(),
            output_dir.display()
        )));
    }
    Ok(outputs)
}

fn choose_size_split_times(
    layout: &PacketLayout,
    target_bytes: u64,
    input_size: u64,
) -> Result<Vec<String>> {
    if input_size <= target_bytes {
        return Ok(Vec::new());
    }
    let payload_goal = u128::from(target_bytes)
        .checked_mul(layout.total_payload_bytes)
        .and_then(|value| value.checked_div(u128::from(input_size)))
        .filter(|value| *value > 0)
        .ok_or_else(|| AppError::new("无法根据媒体包计算目标分片大小"))?;

    let mut selected = Vec::new();
    let mut last_offset = 0_u128;
    let mut next_index = 0_usize;
    while layout.total_payload_bytes.saturating_sub(last_offset) > payload_goal {
        while next_index < layout.keyframes.len()
            && layout.keyframes[next_index].payload_offset <= last_offset
        {
            next_index += 1;
        }
        if next_index == layout.keyframes.len() {
            break;
        }

        let desired = last_offset
            .checked_add(payload_goal)
            .unwrap_or(layout.total_payload_bytes);
        let mut after_index = next_index;
        while after_index < layout.keyframes.len()
            && layout.keyframes[after_index].payload_offset < desired
        {
            after_index += 1;
        }
        let before_index = (after_index > next_index).then_some(after_index - 1);
        let after_index = (after_index < layout.keyframes.len()).then_some(after_index);
        let mut selected_index =
            closest_boundary(&layout.keyframes, desired, before_index, after_index)
                .ok_or_else(|| AppError::new("找不到可用的关键帧分片边界"))?;

        let mut boundary = layout.keyframes[selected_index];
        let minimum_tail = payload_goal / 2;
        if layout
            .total_payload_bytes
            .saturating_sub(boundary.payload_offset)
            < minimum_tail
        {
            if let Some(index) = before_index.filter(|index| *index != selected_index) {
                let earlier = layout.keyframes[index];
                if layout
                    .total_payload_bytes
                    .saturating_sub(earlier.payload_offset)
                    >= minimum_tail
                {
                    selected_index = index;
                    boundary = earlier;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        if boundary.payload_offset <= last_offset {
            break;
        }

        selected.push(format_micros(boundary.elapsed_micros));
        last_offset = boundary.payload_offset;
        next_index = selected_index + 1;
    }
    Ok(selected)
}

fn closest_boundary(
    boundaries: &[KeyframeBoundary],
    desired: u128,
    before: Option<usize>,
    after: Option<usize>,
) -> Option<usize> {
    match (before, after) {
        (Some(before), Some(after)) => {
            let before_error = boundaries[before].payload_offset.abs_diff(desired);
            let after_error = boundaries[after].payload_offset.abs_diff(desired);
            Some(if before_error <= after_error {
                before
            } else {
                after
            })
        }
        (Some(index), None) | (None, Some(index)) => Some(index),
        (None, None) => None,
    }
}

fn format_micros(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn prepare_periodic_outputs(
    output_dir: &Path,
    prefix: &str,
    extension: &str,
    overwrite: bool,
) -> Result<()> {
    create_output_directory(output_dir)?;
    let existing = matching_numbered_outputs(output_dir, prefix, extension)?;
    if let Some((_, path)) = existing.first().filter(|_| !overwrite) {
        return Err(AppError::new(format!(
            "输出文件 `{}` 已存在；使用 --overwrite 覆盖该前缀的已有分段",
            path.display()
        )));
    }
    for (_, path) in existing {
        fs::remove_file(&path).map_err(|error| {
            AppError::new(format!("无法覆盖输出文件 `{}`：{error}", path.display()))
        })?;
    }
    Ok(())
}

fn discover_periodic_outputs(
    output_dir: &Path,
    prefix: &str,
    extension: &str,
) -> Result<Vec<PathBuf>> {
    let outputs = matching_numbered_outputs(output_dir, prefix, extension)?;
    if outputs.is_empty() {
        return Err(AppError::new(format!(
            "FFmpeg 未在 `{}` 中生成任何分段",
            output_dir.display()
        )));
    }
    for (position, (index, _)) in outputs.iter().enumerate() {
        let expected = position + 1;
        if *index != expected {
            return Err(AppError::new(format!(
                "输出分段编号不连续：预期 {expected:03}，实际为 {index:03}"
            )));
        }
    }
    Ok(outputs.into_iter().map(|(_, path)| path).collect())
}

fn matching_numbered_outputs(
    output_dir: &Path,
    prefix: &str,
    extension: &str,
) -> Result<Vec<(usize, PathBuf)>> {
    let name_prefix = format!("{prefix}-");
    let name_suffix = format!(".{extension}");
    let entries = fs::read_dir(output_dir).map_err(|error| {
        AppError::new(format!(
            "无法读取输出目录 `{}`：{error}",
            output_dir.display()
        ))
    })?;
    let mut outputs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::new(format!(
                "无法读取输出目录 `{}` 中的项目：{error}",
                output_dir.display()
            ))
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(index) = numbered_output_index(file_name, &name_prefix, &name_suffix) else {
            continue;
        };
        outputs.push((index, entry.path()));
    }
    outputs.sort_unstable_by_key(|(index, _)| *index);
    Ok(outputs)
}

fn numbered_output_index(file_name: &str, name_prefix: &str, name_suffix: &str) -> Option<usize> {
    let digits = file_name
        .strip_prefix(name_prefix)?
        .strip_suffix(name_suffix)?;
    if digits.len() < 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index = digits.parse::<usize>().ok()?;
    (index > 0 && digits == format!("{index:03}")).then_some(index)
}

fn reject_split_input_collision(
    input: &Path,
    output_dir: &Path,
    prefix: &str,
    extension: &str,
) -> Result<()> {
    let input_parent = input
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|error| {
            AppError::new(format!("无法解析输入目录 `{}`：{error}", input.display()))
        })?;
    let canonical_output_dir = output_dir.canonicalize().map_err(|error| {
        AppError::new(format!(
            "无法解析输出目录 `{}`：{error}",
            output_dir.display()
        ))
    })?;
    let input_name = input
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::new("输入文件名必须是有效 UTF-8 文本"))?;
    let name_prefix = format!("{prefix}-");
    let name_suffix = format!(".{extension}");

    if input_parent == canonical_output_dir
        && numbered_output_index(input_name, &name_prefix, &name_suffix).is_some()
    {
        return Err(AppError::new(format!(
            "输入文件 `{}` 与拆分输出命名范围冲突；请更换 --output-dir 或 --prefix",
            input.display()
        )));
    }
    Ok(())
}

fn create_output_directory(output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir).map_err(|error| {
        AppError::new(format!(
            "无法创建输出目录 `{}`：{error}",
            output_dir.display()
        ))
    })
}

pub fn concat(ffmpeg: &Ffmpeg, request: ConcatArgs) -> Result<()> {
    if request.output.extension().is_none() {
        return Err(AppError::new("输出视频必须带文件扩展名，以确定封装格式"));
    }
    if request.output.exists() && !request.overwrite {
        return Err(AppError::new(format!(
            "输出文件 `{}` 已存在；使用 --overwrite 覆盖",
            request.output.display()
        )));
    }

    let mut canonical_inputs = Vec::with_capacity(request.inputs.len());
    for input in &request.inputs {
        require_input_file(input)?;
        let canonical = input.canonicalize().map_err(|error| {
            AppError::new(format!("无法解析输入文件 `{}`：{error}", input.display()))
        })?;
        canonical_inputs.push(canonical);
    }
    reject_output_as_input(&request.output, &canonical_inputs)?;
    create_parent_directory(&request.output)?;

    let concat_list = ConcatList::create(&canonical_inputs)?;
    let args = vec![
        "-hide_banner".into(),
        overwrite_flag(request.overwrite).into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        concat_list.path().as_os_str().to_owned(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        request.output.clone().into_os_string(),
    ];
    ffmpeg.run(&args)?;
    if !request.output.is_file() {
        return Err(AppError::new(format!(
            "FFmpeg 未生成输出文件 `{}`",
            request.output.display()
        )));
    }
    Ok(())
}

fn require_input_file(path: &Path) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "输入文件 `{}` 不存在或不是普通文件",
            path.display()
        )))
    }
}

fn ensure_strictly_increasing(times: &[crate::time::Timestamp]) -> Result<()> {
    if let Some(pair) = times.windows(2).find(|pair| pair[0] >= pair[1]) {
        return Err(AppError::new(format!(
            "拆分点必须严格递增，但 {} 不早于 {}",
            pair[0], pair[1]
        )));
    }
    Ok(())
}

fn prepare_split_outputs(output_dir: &Path, outputs: &[PathBuf], overwrite: bool) -> Result<()> {
    fs::create_dir_all(output_dir).map_err(|error| {
        AppError::new(format!(
            "无法创建输出目录 `{}`：{error}",
            output_dir.display()
        ))
    })?;
    for path in outputs {
        if path.exists() && !overwrite {
            return Err(AppError::new(format!(
                "输出文件 `{}` 已存在；使用 --overwrite 覆盖",
                path.display()
            )));
        }
        if path.exists() {
            fs::remove_file(path).map_err(|error| {
                AppError::new(format!("无法覆盖输出文件 `{}`：{error}", path.display()))
            })?;
        }
    }
    Ok(())
}

fn segment_pattern(output_dir: &Path, prefix: &str, extension: &str) -> Result<PathBuf> {
    let directory = output_dir
        .to_str()
        .ok_or_else(|| AppError::new("FFmpeg segment 输出目录必须是有效 UTF-8 路径"))?
        .replace('%', "%%");
    let prefix = prefix.replace('%', "%%");
    let extension = extension.replace('%', "%%");
    Ok(PathBuf::from(format!(
        "{directory}{}{prefix}-%03d.{extension}",
        std::path::MAIN_SEPARATOR
    )))
}

fn utf8_component(value: Option<&std::ffi::OsStr>, missing: &str) -> Result<String> {
    value
        .ok_or_else(|| AppError::new(missing))?
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| AppError::new("文件名和扩展名必须是有效 UTF-8 文本"))
}

fn overwrite_flag(overwrite: bool) -> &'static str {
    if overwrite { "-y" } else { "-n" }
}

fn create_parent_directory(output: &Path) -> Result<()> {
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|error| {
            AppError::new(format!("无法创建目录 `{}`：{error}", parent.display()))
        })?;
    }
    Ok(())
}

fn reject_output_as_input(output: &Path, inputs: &[PathBuf]) -> Result<()> {
    let absolute_output = if output.exists() {
        output.canonicalize().map_err(|error| {
            AppError::new(format!(
                "无法解析输出文件路径 `{}`：{error}",
                output.display()
            ))
        })?
    } else if output.is_absolute() {
        output.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| AppError::new(format!("无法获取当前目录：{error}")))?
            .join(output)
    };
    if inputs.iter().any(|input| input == &absolute_output) {
        return Err(AppError::new("输出文件不能同时作为输入文件"));
    }
    Ok(())
}

struct ConcatList {
    path: PathBuf,
}

impl ConcatList {
    fn create(inputs: &[PathBuf]) -> Result<Self> {
        let temp_dir = env::temp_dir();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for attempt in 0..100_u8 {
            let path = temp_dir.join(format!(
                "vidsplice-{}-{nonce}-{attempt}.ffconcat",
                process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    write_concat_list(&mut file, inputs, &path)?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(AppError::new(format!(
                        "无法创建临时拼接清单 `{}`：{error}",
                        path.display()
                    )));
                }
            }
        }
        Err(AppError::new("无法分配唯一的临时拼接清单"))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ConcatList {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn write_concat_list(file: &mut File, inputs: &[PathBuf], path: &Path) -> Result<()> {
    writeln!(file, "ffconcat version 1.0").map_err(|error| concat_list_write_error(path, error))?;
    for input in inputs {
        let value = input
            .to_str()
            .ok_or_else(|| AppError::new("拼接输入路径必须是有效 UTF-8 文本"))?;
        if value.contains(['\n', '\r']) {
            return Err(AppError::new("拼接输入路径不能包含换行符"));
        }
        // ffconcat 的单引号本身不能放在单引号字符串内：先结束引号，
        // 用反斜杠转义该字符，再重新开始单引号字符串。
        let escaped = value.replace('\'', "'\\''");
        writeln!(file, "file '{escaped}'").map_err(|error| concat_list_write_error(path, error))?;
    }
    file.flush()
        .map_err(|error| concat_list_write_error(path, error))
}

fn concat_list_write_error(path: &Path, error: std::io::Error) -> AppError {
    AppError::new(format!(
        "写入临时拼接清单 `{}` 失败：{error}",
        path.display()
    ))
}
