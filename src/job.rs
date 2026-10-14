//! ダウンロード処理の本体。1 本のワーカースレッドで直列に実行する。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::api;
use crate::hls;
use crate::net::{Client, HttpStatus, Kind, is_cancelled};
use crate::render;
use crate::util;

#[derive(Debug, Clone)]
pub struct Options {
    /// プロフィール URL またはユーザー ID。
    pub user: String,
    /// この下に `<ユーザーID>/` フォルダを作る。
    pub out_root: PathBuf,
    pub comments: bool,
    pub videos: bool,
    /// 保存済みの作品も情報を取り直す (ファイルは既存のものを再利用)。
    pub refresh: bool,
    /// 動作確認用: 先頭 N 件だけ処理する。
    pub limit: Option<usize>,
    /// 一覧を取得せず、指定した作品 ID だけ処理する。
    pub only_ids: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            user: String::new(),
            out_root: default_out_root(),
            comments: true,
            videos: true,
            refresh: false,
            limit: None,
            only_ids: Vec::new(),
        }
    }
}

pub fn default_out_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aipictors-archive")
}

pub enum Event {
    Log(String),
    Progress {
        phase: String,
        done: usize,
        total: usize,
    },
    Finished(std::result::Result<Summary, String>),
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub index: PathBuf,
    pub cancelled: bool,
    pub saved: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// 進捗とログの出口。GUI ではチャンネル、CLI では標準出力につなぐ。
pub struct Reporter {
    sink: Box<dyn Fn(Event) + Send + Sync>,
    log_file: Mutex<Option<fs::File>>,
}

impl Reporter {
    pub fn new(sink: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self {
            sink: Box::new(sink),
            log_file: Mutex::new(None),
        }
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        if let Ok(mut f) = self.log_file.lock()
            && let Some(f) = f.as_mut()
        {
            let _ = writeln!(f, "[{}] {}", util::format_time(util::now_unix()), msg);
        }
        (self.sink)(Event::Log(msg));
    }

    pub fn progress(&self, phase: impl Into<String>, done: usize, total: usize) {
        (self.sink)(Event::Progress {
            phase: phase.into(),
            done,
            total,
        });
    }

    pub fn finish(&self, r: std::result::Result<Summary, String>) {
        (self.sink)(Event::Finished(r));
    }

    fn open_log(&self, path: &Path) {
        if let Ok(f) = fs::OpenOptions::new().create(true).append(true).open(path)
            && let Ok(mut g) = self.log_file.lock()
        {
            *g = Some(f);
        }
    }
}

pub fn run(opts: &Options, cancel: Arc<AtomicBool>, reporter: Arc<Reporter>) -> Result<Summary> {
    let login = api::parse_user_input(&opts.user).ok_or_else(|| {
        anyhow!("プロフィール URL (https://www.aipictors.com/users/○○) を入力してください")
    })?;
    let client = Client::new(cancel, reporter.clone());
    let r = &reporter;

    r.progress("ユーザー情報を取得中", 0, 0);
    let user = api::fetch_user(&client, &login)?;
    let user_id = user
        .get("id")
        .and_then(api::id_string)
        .ok_or_else(|| anyhow!("ユーザー ID が取得できませんでした"))?;
    let login = user
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or(&login)
        .to_string();

    let root = opts.out_root.join(util::safe_file_name(&login));
    for d in ["data", "user", "works", "stickers", "icons"] {
        fs::create_dir_all(root.join(d))
            .with_context(|| format!("{} を作成できません", root.display()))?;
    }
    r.open_log(&root.join("aipicdl.log"));
    r.log(format!(
        "ユーザー: {} (@{login}, id {user_id}) → {}",
        user.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        root.display()
    ));

    // プロフィール
    let mut user_saved = user.clone();
    let mut local = serde_json::Map::new();
    for (key, stem) in [("iconUrl", "icon"), ("headerImageUrl", "header")] {
        if let Some(url) = non_empty(&user, key)
            && let Some(name) = save_url(
                &client,
                &format!("プロフィール画像 ({stem})"),
                url,
                &root.join("user"),
                stem,
            )?
        {
            local.insert(stem.to_string(), json!(format!("user/{name}")));
        }
    }
    local.insert("archivedAt".into(), json!(util::now_unix()));
    user_saved["_local"] = Value::Object(local);
    write_json(&root.join("data/user.json"), &user_saved)?;

    let ids = if opts.only_ids.is_empty() {
        list_work_ids(&client, opts, &root, &user_id)?
    } else {
        opts.only_ids.clone()
    };

    // 作品ごと
    let total = ids.len();
    let mut summary = Summary {
        index: root.join("index.html"),
        cancelled: false,
        saved: 0,
        skipped: 0,
        failed: 0,
    };
    for (i, id) in ids.iter().enumerate() {
        r.progress("作品を保存中", i, total);
        let dir = root.join("works").join(util::safe_file_name(id));
        if dir.join("work.json").exists() && !opts.refresh {
            summary.skipped += 1;
            continue;
        }
        match save_work(&client, opts, &root, &dir, id) {
            Ok(()) => summary.saved += 1,
            Err(e) if is_cancelled(&e) => {
                summary.cancelled = true;
                break;
            }
            Err(e) => {
                summary.failed += 1;
                r.log(format!("  × 作品 {id}: {e:#}"));
            }
        }
    }
    if !summary.cancelled {
        r.progress("作品を保存中", total, total);
    }

    r.progress("HTML を生成中", 0, 0);
    let rendered = render::render_archive(&root).context("HTML の生成に失敗")?;
    r.log(format!(
        "完了: 新規/更新 {} 件, スキップ {} 件, 失敗 {} 件 (アーカイブ内 {} 作品){}",
        summary.saved,
        summary.skipped,
        summary.failed,
        rendered,
        if summary.cancelled {
            " — 中断しました"
        } else {
            ""
        }
    ));
    Ok(summary)
}

fn list_work_ids(
    client: &Client,
    opts: &Options,
    root: &Path,
    user_id: &str,
) -> Result<Vec<String>> {
    let r = client.reporter();
    let expected = api::fetch_works_count(client, user_id)?;
    r.log(format!("公開作品数: {expected}"));
    let mut ids: Vec<String> = Vec::new();
    let mut offset = 0;
    loop {
        r.progress("作品一覧を取得中", ids.len(), expected);
        let page = api::fetch_work_ids_page(client, user_id, offset)?;
        let n = page.len();
        for id in page {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        offset += n;
        if n < api::LIST_PAGE || opts.limit.is_some_and(|l| ids.len() >= l) {
            break;
        }
    }
    if opts.limit.is_none() && ids.len() != expected {
        r.log(format!(
            "  ! 一覧で取得できた件数 ({}) が作品数 ({expected}) と一致しません",
            ids.len()
        ));
    }
    write_json(
        &root.join("data/work_ids.json"),
        &json!({ "fetchedAt": util::now_unix(), "ids": ids }),
    )?;
    if let Some(l) = opts.limit {
        ids.truncate(l);
    }

    Ok(ids)
}

fn save_work(client: &Client, opts: &Options, root: &Path, dir: &Path, id: &str) -> Result<()> {
    let mut work = api::fetch_work(client, id, opts.comments)?;
    let comments = work
        .as_object_mut()
        .and_then(|o| o.remove("comments"))
        .and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        });
    let title = work
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = work
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    client.check_cancel()?;
    fs::create_dir_all(dir)?;
    let mut local = serde_json::Map::new();
    let mut notes: Vec<String> = Vec::new();

    // 画像 (動画・コラム・小説ではサムネイル / 表紙)
    let mut urls: Vec<String> = Vec::new();
    if let Some(u) = non_empty(&work, "imageURL") {
        urls.push(u.to_string());
    }
    if let Some(subs) = work.get("subWorks").and_then(|v| v.as_array()) {
        for s in subs {
            if let Some(u) = non_empty(s, "imageUrl") {
                urls.push(u.to_string());
            }
        }
    }
    let mut images = Vec::new();
    for (n, url) in urls.iter().enumerate() {
        let stem = format!("{n:02}");
        match save_url(
            client,
            &format!("作品 {id} の画像 {}", n + 1),
            url,
            dir,
            &stem,
        )? {
            Some(name) => images.push(json!(name)),
            None => notes.push(format!("画像 {} が見つかりません (404)", n + 1)),
        }
    }
    if let Some(first) = images.first().and_then(|v| v.as_str()) {
        let thumb = dir.join("thumb.jpg");
        if !thumb.exists()
            && let Err(e) = util::make_thumbnail(&dir.join(first), &thumb)
        {
            client_log(
                client,
                format!("  ! 作品 {id}: サムネイル生成に失敗: {e:#}"),
            );
        }
        if thumb.exists() {
            local.insert("thumb".into(), json!("thumb.jpg"));
        }
    }
    local.insert("images".into(), Value::Array(images));

    // 動画
    let stream_uid = non_empty(&work, "streamUid")
        .map(str::to_string)
        .or_else(|| {
            non_empty(&work, "url")
                .filter(|u| u.contains("videodelivery.net"))
                .and_then(|u| {
                    u.trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .map(str::to_string)
                })
        });
    if opts.videos
        && let Some(uid) = stream_uid
    {
        match save_video(client, &uid, dir) {
            Ok(files) => {
                for (k, v) in files {
                    local.insert(k, json!(v));
                }
            }
            Err(e) if is_cancelled(&e) => return Err(e),
            Err(e) => notes.push(format!("動画を保存できませんでした: {e:#}")),
        }
    }

    // 本文 (コラム・小説)
    if let Some(md_url) = non_empty(&work, "mdUrl").map(str::to_string) {
        let path = dir.join("text.md");
        let text = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            let t = client.get_text(Kind::File, &format!("作品 {id} の本文"), &md_url)?;
            write_atomic(&path, t.as_bytes())?;
            t
        };
        local.insert("text".into(), json!("text.md"));
        let mut map = serde_json::Map::new();
        for (n, url) in util::markdown_image_urls(&text).into_iter().enumerate() {
            match save_url(
                client,
                &format!("作品 {id} の本文画像"),
                &url,
                dir,
                &format!("md_{n:02}"),
            ) {
                Ok(Some(name)) => {
                    map.insert(url, json!(name));
                }
                Ok(None) => notes.push(format!("本文中の画像が見つかりません: {url}")),
                Err(e) if is_cancelled(&e) => return Err(e),
                Err(e) => notes.push(format!("本文中の画像を保存できませんでした: {e:#}")),
            }
        }
        if !map.is_empty() {
            local.insert("textImages".into(), Value::Object(map));
        }
    }

    // コメント
    if let Some(comments) = comments.filter(|c| !c.is_empty()) {
        for c in &comments {
            save_comment_assets(client, root, c)?;
            if let Some(rs) = c.get("responses").and_then(|v| v.as_array()) {
                for rc in rs {
                    save_comment_assets(client, root, rc)?;
                }
            }
        }
        write_json(&dir.join("comments.json"), &Value::Array(comments))?;
        local.insert("comments".into(), json!("comments.json"));
    }

    local.insert("archivedAt".into(), json!(util::now_unix()));
    if !notes.is_empty() {
        for n in &notes {
            client_log(client, format!("  ! 作品 {id}: {n}"));
        }
        local.insert("notes".into(), json!(notes));
    }
    work["_local"] = Value::Object(local);
    // work.json は最後に書く (= この作品の保存完了の印)。
    write_json(&dir.join("work.json"), &work)?;
    client_log(
        client,
        format!("  ✓ {id} [{kind}] {}", util::truncate_chars(&title, 40)),
    );
    Ok(())
}

fn client_log(client: &Client, msg: String) {
    client.reporter().log(msg);
}

fn save_comment_assets(client: &Client, root: &Path, c: &Value) -> Result<()> {
    if let Some(sticker) = c.get("sticker")
        && let (Some(sid), Some(url)) = (
            sticker.get("id").and_then(api::id_string),
            non_empty(sticker, "imageUrl"),
        )
    {
        soft(
            client,
            save_url(
                client,
                "スタンプ画像",
                url,
                &root.join("stickers"),
                &util::safe_file_name(&sid),
            ),
        )?;
    }
    if let Some(user) = c.get("user")
        && let (Some(uid), Some(url)) = (
            user.get("id").and_then(api::id_string),
            non_empty(user, "iconUrl"),
        )
    {
        soft(
            client,
            save_url(
                client,
                "コメント投稿者のアイコン",
                url,
                &root.join("icons"),
                &util::safe_file_name(&uid),
            ),
        )?;
    }
    Ok(())
}

/// 付随ファイルの失敗は作品全体の失敗にしない (中断だけは伝える)。
fn soft(client: &Client, r: Result<Option<String>>) -> Result<()> {
    match r {
        Ok(_) => Ok(()),
        Err(e) if is_cancelled(&e) => Err(e),
        Err(e) => {
            client_log(client, format!("  ! {e:#}"));
            Ok(())
        }
    }
}

/// Cloudflare Stream の HLS から最高画質の映像と音声を取得し、MP4 にまとめる。
fn save_video(client: &Client, uid: &str, dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    if dir.join("video.mp4").exists() {
        files.insert("video".into(), "video.mp4".into());
        if dir.join("audio.m4a").exists() {
            files.insert("audio".into(), "audio.m4a".into());
        }
        return Ok(files);
    }
    let master_url = format!("https://videodelivery.net/{uid}/manifest/video.m3u8");
    let master =
        hls::parse_master(&client.get_text(Kind::File, "動画のプレイリスト", &master_url)?);
    let (variant, audio) = master
        .best()
        .ok_or_else(|| anyhow!("プレイリストに映像がありません"))?;
    let (vinit, vsegs, duration) = fetch_rendition(client, &master_url, &variant.uri, "映像")?;
    let Some(audio) = audio else {
        write_atomic(&dir.join("video.mp4"), &concat(vinit, &vsegs))?;
        files.insert("video".into(), "video.mp4".into());
        return Ok(files);
    };
    let (ainit, asegs, adur) = fetch_rendition(client, &master_url, &audio.uri, "音声")?;
    match hls::mux(
        vinit.clone(),
        &vsegs,
        ainit.clone(),
        &asegs,
        duration.max(adur),
    ) {
        Ok(mp4) => {
            write_atomic(&dir.join("video.mp4"), &mp4)?;
            files.insert("video".into(), "video.mp4".into());
        }
        Err(e) => {
            // 合成できなければ映像と音声を別ファイルのまま残す。
            client_log(
                client,
                format!("  ! 映像と音声の合成に失敗したため別々に保存します: {e:#}"),
            );
            write_atomic(&dir.join("audio.m4a"), &concat(ainit, &asegs))?;
            write_atomic(&dir.join("video.mp4"), &concat(vinit, &vsegs))?;
            files.insert("video".into(), "video.mp4".into());
            files.insert("audio".into(), "audio.m4a".into());
        }
    }
    Ok(files)
}

fn fetch_rendition(
    client: &Client,
    master_url: &str,
    uri: &str,
    label: &str,
) -> Result<(Vec<u8>, Vec<Vec<u8>>, f64)> {
    let pl_url = util::join_url(master_url, uri)?;
    let pl = hls::parse_media(&client.get_text(
        Kind::File,
        &format!("{label}のプレイリスト"),
        &pl_url,
    )?);
    let init_uri = pl
        .init
        .as_deref()
        .ok_or_else(|| anyhow!("{label}: 初期化セグメントがありません (fMP4 以外の形式)"))?;
    let init = client
        .get(
            Kind::File,
            &format!("{label}の初期化セグメント"),
            &util::join_url(&pl_url, init_uri)?,
        )?
        .bytes;
    let mut segs = Vec::with_capacity(pl.segments.len());
    for (i, s) in pl.segments.iter().enumerate() {
        let what = format!("{label}セグメント {}/{}", i + 1, pl.segments.len());
        segs.push(
            client
                .get(Kind::File, &what, &util::join_url(&pl_url, s)?)?
                .bytes,
        );
    }
    Ok((init, segs, pl.duration))
}

fn concat(init: Vec<u8>, segs: &[Vec<u8>]) -> Vec<u8> {
    let mut v = init;
    for s in segs {
        v.extend_from_slice(s);
    }
    v
}

fn non_empty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `dir/stem.*` が既にあれば再取得しない。404 は `Ok(None)`。
fn save_url(
    client: &Client,
    what: &str,
    url: &str,
    dir: &Path,
    stem: &str,
) -> Result<Option<String>> {
    if let Some(name) = util::find_by_stem(dir, stem) {
        return Ok(Some(name));
    }
    let d = match client.get(Kind::File, what, url) {
        Ok(d) => d,
        Err(e)
            if matches!(
                e.downcast_ref::<HttpStatus>(),
                Some(HttpStatus(404 | 403 | 410))
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let ext = util::detect_ext(&d.bytes, d.content_type.as_deref(), url);
    let name = format!("{stem}.{ext}");
    fs::create_dir_all(dir)?;
    write_atomic(&dir.join(&name), &d.bytes)?;
    Ok(Some(name))
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes).with_context(|| format!("{} に書き込めません", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("{} に書き込めません", path.display()))?;
    Ok(())
}

pub fn write_json(path: &Path, v: &Value) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(v)?)
}
