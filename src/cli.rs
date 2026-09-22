//! Command-line parsing and help output.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::{AppError, Result};
use crate::size::TargetSize;
use crate::time::{SegmentDuration, Timestamp};

pub const HELP: &str = r#"vidsplice - 使用 FFmpeg 无损拆分和拼接视频

用法:
  vidsplice split <输入视频> (--at <时间点>... | --every <时长> | --size <大小>) [选项]
  vidsplice concat <输出视频> <输入视频> <输入视频>... [选项]
  vidsplice --help
  vidsplice --version

命令:
  split   按指定时间点、固定时长或目标大小拆分视频
  concat  按参数顺序拼接至少两个编码参数兼容的视频

split 选项:
  -t, --at <时间点>       拆分点；可重复，或用逗号分隔
      --every <时长>      按固定时长分段，如 9m、540s、1h 或 00:09:00
      --size <大小>       按目标大小近似分片，如 100MB 或 100MiB
  -o, --output-dir <目录> 输出目录（默认：输入文件旁的 <文件名>_parts）
      --prefix <名称>     输出文件名前缀（默认：输入文件名）
  -y, --overwrite         覆盖已有输出

  --at、--every 与 --size 必须且只能指定一个。

concat 选项:
  -y, --overwrite         覆盖已有输出

环境变量:
  FFMPEG_BIN              FFmpeg 可执行文件路径（默认：ffmpeg）
  FFPROBE_BIN             ffprobe 可执行文件路径（默认：ffprobe）

示例:
  vidsplice split movie.mp4 --size 100MB
  vidsplice split movie.mp4 --every 9m
  vidsplice split movie.mp4 --at 00:30 --at 01:20.500
  vidsplice concat joined.mp4 part-001.mp4 part-002.mp4
"#;

#[derive(Debug)]
pub enum Action {
    Help,
    Version,
    Run(Command),
}

#[derive(Debug)]
pub enum Command {
    Split(SplitArgs),
    Concat(ConcatArgs),
}

#[derive(Debug)]
pub enum SplitMode {
    At(Vec<Timestamp>),
    Every(SegmentDuration),
    Size(TargetSize),
}

#[derive(Debug)]
pub struct SplitArgs {
    pub input: PathBuf,
    pub mode: SplitMode,
    pub output_dir: Option<PathBuf>,
    pub prefix: Option<String>,
    pub overwrite: bool,
}

#[derive(Debug)]
pub struct ConcatArgs {
    pub output: PathBuf,
    pub inputs: Vec<PathBuf>,
    pub overwrite: bool,
}

pub fn parse() -> Result<Action> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.is_empty() {
        return Ok(Action::Help);
    }

    let first = args[0].to_string_lossy();
    if matches!(first.as_ref(), "-h" | "--help" | "help") {
        return Ok(Action::Help);
    }
    if matches!(first.as_ref(), "-V" | "--version" | "version") {
        return Ok(Action::Version);
    }
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
    {
        return Ok(Action::Help);
    }

    match first.as_ref() {
        "split" => parse_split(&args[1..]).map(|args| Action::Run(Command::Split(args))),
        "concat" => parse_concat(&args[1..]).map(|args| Action::Run(Command::Concat(args))),
        command => Err(AppError::new(format!(
            "未知命令 `{command}`；运行 `vidsplice --help` 查看用法"
        ))),
    }
}

fn parse_split(args: &[OsString]) -> Result<SplitArgs> {
    let mut input = None;
    let mut times = Vec::new();
    let mut every = None;
    let mut size = None;
    let mut output_dir = None;
    let mut prefix = None;
    let mut overwrite = false;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if !positional_only && argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only && matches!(argument.as_ref(), "-t" | "--at") {
            let value = required_value(args, index, argument.as_ref())?;
            let value = value
                .to_str()
                .ok_or_else(|| AppError::new("时间点必须是有效 UTF-8 文本"))?;
            for item in value.split(',') {
                times.push(item.parse()?);
            }
            index += 2;
            continue;
        }
        if !positional_only && argument == "--every" {
            if every.is_some() {
                return Err(AppError::new("--every 只能指定一次"));
            }
            let value = required_value(args, index, "--every")?
                .to_str()
                .ok_or_else(|| AppError::new("分段时长必须是有效 UTF-8 文本"))?;
            every = Some(value.parse()?);
            index += 2;
            continue;
        }
        if !positional_only && argument == "--size" {
            if size.is_some() {
                return Err(AppError::new("--size 只能指定一次"));
            }
            let value = required_value(args, index, "--size")?
                .to_str()
                .ok_or_else(|| AppError::new("目标大小必须是有效 UTF-8 文本"))?;
            size = Some(value.parse()?);
            index += 2;
            continue;
        }
        if !positional_only && matches!(argument.as_ref(), "-o" | "--output-dir") {
            output_dir = Some(PathBuf::from(required_value(
                args,
                index,
                argument.as_ref(),
            )?));
            index += 2;
            continue;
        }
        if !positional_only && argument == "--prefix" {
            let value = required_value(args, index, "--prefix")?
                .to_str()
                .ok_or_else(|| AppError::new("输出前缀必须是有效 UTF-8 文本"))?
                .to_owned();
            validate_prefix(&value)?;
            prefix = Some(value);
            index += 2;
            continue;
        }
        if !positional_only && matches!(argument.as_ref(), "-y" | "--overwrite") {
            overwrite = true;
            index += 1;
            continue;
        }
        if !positional_only && argument.starts_with('-') {
            return Err(AppError::new(format!("split 的未知选项 `{argument}`")));
        }
        if input.replace(PathBuf::from(&args[index])).is_some() {
            return Err(AppError::new("split 只能指定一个输入视频"));
        }
        index += 1;
    }

    let input = input.ok_or_else(|| AppError::new("split 缺少输入视频"))?;
    let selected_modes =
        usize::from(!times.is_empty()) + usize::from(every.is_some()) + usize::from(size.is_some());
    if selected_modes == 0 {
        return Err(AppError::new("split 必须指定 --at、--every 或 --size"));
    }
    if selected_modes > 1 {
        return Err(AppError::new("--at、--every 与 --size 不能同时使用"));
    }
    let mode = if !times.is_empty() {
        SplitMode::At(times)
    } else if let Some(duration) = every {
        SplitMode::Every(duration)
    } else {
        SplitMode::Size(size.expect("selected size mode must contain a value"))
    };
    Ok(SplitArgs {
        input,
        mode,
        output_dir,
        prefix,
        overwrite,
    })
}

fn parse_concat(args: &[OsString]) -> Result<ConcatArgs> {
    let mut paths = Vec::new();
    let mut overwrite = false;
    let mut positional_only = false;

    for arg in args {
        let value = arg.to_string_lossy();
        if !positional_only && value == "--" {
            positional_only = true;
        } else if !positional_only && matches!(value.as_ref(), "-y" | "--overwrite") {
            overwrite = true;
        } else if !positional_only && value.starts_with('-') {
            return Err(AppError::new(format!("concat 的未知选项 `{value}`")));
        } else {
            paths.push(PathBuf::from(arg));
        }
    }

    if paths.len() < 3 {
        return Err(AppError::new("concat 需要一个输出视频和至少两个输入视频"));
    }
    Ok(ConcatArgs {
        output: paths.remove(0),
        inputs: paths,
        overwrite,
    })
}

fn required_value<'a>(args: &'a [OsString], index: usize, option: &str) -> Result<&'a OsString> {
    args.get(index + 1)
        .ok_or_else(|| AppError::new(format!("选项 `{option}` 缺少值")))
}

fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty()
        || matches!(prefix, "." | "..")
        || prefix.contains('/')
        || prefix.contains('\\')
    {
        return Err(AppError::new(
            "输出前缀不能为空、`.`、`..`，也不能包含路径分隔符",
        ));
    }
    Ok(())
}
