#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod gui;
mod hls;
mod job;
mod net;
mod render;
mod util;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const USAGE: &str = "\
使い方:
  aipicdl                         GUI を起動
  aipicdl <プロフィールURL> [オプション]   コマンドラインで実行

オプション:
  --out <DIR>            保存先 (この下に <ユーザーID> フォルダを作る)
  --api-interval <秒>    API リクエストの間隔 (既定 3)
  --file-interval <秒>   ファイル取得の間隔 (既定 1)
  --no-comments          コメントを保存しない
  --no-videos            動画を保存しない
  --refresh              保存済みの作品も情報を取り直す
  --limit <N>            先頭 N 件だけ処理する (動作確認用)
  --ids <ID,ID,...>      指定した作品 ID だけ処理する
  --render-only <DIR>    通信せず、保存済みフォルダの HTML だけ作り直す
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        if let Err(e) = gui::run() {
            eprintln!("{e}");
        }
        return;
    }
    attach_console();
    std::process::exit(cli(args));
}

/// GUI サブシステムの exe でも、コマンドプロンプトから起動したときは出力を表示する。
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn cli(args: Vec<String>) -> i32 {
    let mut opts = job::Options::default();
    let mut render_only: Option<PathBuf> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Option<String> {
            let v = it.next();
            if v.is_none() {
                eprintln!("{name} には値が必要です");
            }
            v
        };
        match a.as_str() {
            "-h" | "--help" | "/?" => {
                println!("{USAGE}");
                return 0;
            }
            "--out" => match value("--out") {
                Some(v) => opts.out_root = PathBuf::from(v),
                None => return 2,
            },
            "--api-interval" | "--file-interval" | "--limit" => {
                let Some(v) = value(&a) else { return 2 };
                let Ok(n) = v.parse::<f64>() else {
                    eprintln!("{a} の値が数値ではありません: {v}");
                    return 2;
                };
                match a.as_str() {
                    "--api-interval" => opts.api_interval = n,
                    "--file-interval" => opts.file_interval = n,
                    _ => opts.limit = Some(n.max(0.0) as usize),
                }
            }
            "--ids" => match value("--ids") {
                Some(v) => {
                    opts.only_ids = v
                        .split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                }
                None => return 2,
            },
            "--no-comments" => opts.comments = false,
            "--no-videos" => opts.videos = false,
            "--refresh" => opts.refresh = true,
            "--render-only" => match value("--render-only") {
                Some(v) => render_only = Some(PathBuf::from(v)),
                None => return 2,
            },
            s if s.starts_with("--") => {
                eprintln!("不明なオプション: {s}\n\n{USAGE}");
                return 2;
            }
            _ => opts.user = a,
        }
    }

    if let Some(dir) = render_only {
        return match render::render_archive(&dir) {
            Ok(n) => {
                println!(
                    "{n} 作品の HTML を生成しました: {}",
                    dir.join("index.html").display()
                );
                0
            }
            Err(e) => {
                eprintln!("エラー: {e:#}");
                1
            }
        };
    }

    let reporter = Arc::new(job::Reporter::new(|ev| match ev {
        job::Event::Log(s) => println!("{s}"),
        job::Event::Progress { .. } | job::Event::Finished(_) => {}
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    match job::run(&opts, cancel, reporter) {
        Ok(s) => {
            println!("index: {}", s.index.display());
            if s.failed > 0 { 1 } else { 0 }
        }
        Err(e) => {
            eprintln!("エラー: {e:#}");
            1
        }
    }
}
