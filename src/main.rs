mod cli;
mod error;
mod ffmpeg;
mod size;
mod time;
mod video;

use cli::{Action, Command};
use error::Result;
use ffmpeg::Ffmpeg;

fn main() {
    if let Err(error) = run() {
        eprintln!("错误: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match cli::parse()? {
        Action::Help => print!("{}", cli::HELP),
        Action::Version => println!("vidsplice {}", env!("CARGO_PKG_VERSION")),
        Action::Run(command) => {
            let ffmpeg = Ffmpeg::from_environment();
            ffmpeg.ensure_available()?;
            match command {
                Command::Split(args) => {
                    let outputs = video::split(&ffmpeg, args)?;
                    println!("拆分完成，共生成 {} 个文件：", outputs.len());
                    for output in outputs {
                        println!("  {}", output.display());
                    }
                }
                Command::Concat(args) => {
                    let output = args.output.clone();
                    video::concat(&ffmpeg, args)?;
                    println!("拼接完成：{}", output.display());
                }
            }
        }
    }
    Ok(())
}
