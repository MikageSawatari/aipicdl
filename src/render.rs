//! 保存済みデータ (data/user.json, works/*/work.json) から閲覧用の静的 HTML を生成する。
//! ネットワークには一切アクセスしないので、何度でも作り直せる。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::job::{write_atomic, write_json};
use crate::util::{format_date, format_time, html_escape as esc, text_to_html};

const STYLE_CSS: &str = include_str!("../assets/style.css");
const APP_JS: &str = include_str!("../assets/app.js");
const SITE: &str = "https://www.aipictors.com";

struct Work {
    id: String,
    dir: String,
    v: Value,
}

impl Work {
    fn s(&self, key: &str) -> &str {
        str_of(&self.v, key)
    }
    fn n(&self, key: &str) -> i64 {
        self.v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
    }
    fn local(&self, key: &str) -> Option<&Value> {
        self.v.get("_local").and_then(|l| l.get(key))
    }
    fn local_str(&self, key: &str) -> Option<&str> {
        self.local(key).and_then(|x| x.as_str())
    }
    fn images(&self) -> Vec<&str> {
        self.local("images")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default()
    }
    fn title(&self) -> &str {
        let t = self.s("title");
        if t.trim().is_empty() { "(無題)" } else { t }
    }
    fn kind(&self) -> &str {
        match self.s("type") {
            "" => "WORK",
            k => k,
        }
    }
    fn rating(&self) -> &str {
        match self.s("rating") {
            "" => "G",
            r => r,
        }
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn kind_label(k: &str) -> &'static str {
    match k {
        "WORK" => "画像",
        "VIDEO" => "動画",
        "COLUMN" => "コラム",
        "NOVEL" => "小説",
        _ => "その他",
    }
}

/// 戻り値はアーカイブ内の作品数。
pub fn render_archive(root: &Path) -> Result<usize> {
    let user: Value = fs::read(root.join("data/user.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);

    let mut works = Vec::new();
    if let Ok(rd) = fs::read_dir(root.join("works")) {
        for e in rd.flatten() {
            let p = e.path().join("work.json");
            let Ok(bytes) = fs::read(&p) else { continue };
            let v: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("{} を読めません", p.display()))?;
            let id = v
                .get("id")
                .and_then(crate::api::id_string)
                .unwrap_or_else(|| e.file_name().to_string_lossy().into_owned());
            works.push(Work {
                id,
                dir: e.file_name().to_string_lossy().into_owned(),
                v,
            });
        }
    }
    works.sort_by(|a, b| {
        b.n("createdAt")
            .cmp(&a.n("createdAt"))
            .then_with(|| b.id.cmp(&a.id))
    });

    fs::create_dir_all(root.join("assets"))?;
    write_atomic(&root.join("assets/style.css"), STYLE_CSS.as_bytes())?;
    write_atomic(&root.join("assets/app.js"), APP_JS.as_bytes())?;

    let stickers = scan_stems(&root.join("stickers"));
    let icons = scan_stems(&root.join("icons"));
    let albums = albums_by_work(&user);

    for (i, w) in works.iter().enumerate() {
        let newer = i.checked_sub(1).map(|j| &works[j]);
        let older = works.get(i + 1);
        let wdir = root.join("works").join(&w.dir);
        let text = w
            .local_str("text")
            .and_then(|f| fs::read_to_string(wdir.join(f)).ok());
        let comments: Vec<Value> = w
            .local_str("comments")
            .and_then(|f| fs::read(wdir.join(f)).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let ctx = PageCtx {
            user: &user,
            newer,
            older,
            stickers: &stickers,
            icons: &icons,
            albums: &albums,
        };
        let html = work_page(w, &ctx, text.as_deref(), &comments);
        write_atomic(
            &root.join("works").join(&w.dir).join("index.html"),
            html.as_bytes(),
        )?;
    }
    write_atomic(
        &root.join("index.html"),
        index_page(&user, &works, &albums).as_bytes(),
    )?;

    // 他のツールから扱いやすいよう、全作品のメタデータを 1 ファイルにもまとめておく。
    let all: Vec<Value> = works.iter().map(|w| w.v.clone()).collect();
    write_json(&root.join("data/works.json"), &Value::Array(all))?;
    Ok(works.len())
}

fn scan_stems(dir: &Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".part") {
                continue;
            }
            if let Some((stem, _)) = name.rsplit_once('.') {
                m.insert(stem.to_string(), name.clone());
            }
        }
    }
    m
}

struct Album {
    id: String,
    title: String,
}

fn albums_by_work(user: &Value) -> (Vec<Album>, HashMap<String, Vec<usize>>) {
    let mut list = Vec::new();
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    if let Some(arr) = user.get("albums").and_then(|v| v.as_array()) {
        for a in arr {
            let idx = list.len();
            list.push(Album {
                id: a
                    .get("id")
                    .and_then(crate::api::id_string)
                    .unwrap_or_default(),
                title: str_of(a, "title").to_string(),
            });
            for wid in a
                .get("workIds")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(w) = crate::api::id_string(wid) {
                    map.entry(w).or_default().push(idx);
                }
            }
        }
    }
    (list, map)
}

fn head(title: &str, css: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>{}</title>\n<link rel=\"stylesheet\" href=\"{css}\">\n</head>\n",
        esc(title)
    )
}

fn thumb_src(w: &Work, prefix: &str) -> Option<String> {
    let f = w
        .local_str("thumb")
        .map(str::to_string)
        .or_else(|| w.images().first().map(|s| s.to_string()))?;
    Some(format!(
        "{prefix}works/{}/{}",
        url_path(&w.dir),
        url_path(&f)
    ))
}

/// ローカルの相対パスを URL として安全な形にする。
fn url_path(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                o.push(b as char)
            }
            _ => {
                let _ = write!(o, "%{b:02X}");
            }
        }
    }
    o
}

fn index_page(
    user: &Value,
    works: &[Work],
    albums: &(Vec<Album>, HashMap<String, Vec<usize>>),
) -> String {
    let name = str_of(user, "name");
    let login = str_of(user, "login");
    let mut h = head(
        &format!("{name} (@{login}) - Aipictors アーカイブ"),
        "assets/style.css",
    );
    h.push_str("<body class=\"index\">\n");

    // プロフィール
    let local = user.get("_local");
    let local_s = |k: &str| local.and_then(|l| l.get(k)).and_then(|v| v.as_str());
    h.push_str("<header class=\"profile\">\n");
    if let Some(hd) = local_s("header") {
        let _ = writeln!(
            h,
            "<div class=\"banner\" style=\"background-image:url('{}')\"></div>",
            url_path(hd)
        );
    }
    h.push_str("<div class=\"profile-body\">\n");
    if let Some(ic) = local_s("icon") {
        let _ = writeln!(
            h,
            "<img class=\"avatar\" src=\"{}\" alt=\"\">",
            url_path(ic)
        );
    }
    let _ = writeln!(
        h,
        "<div class=\"who\"><h1>{}</h1><div class=\"login\">@{}</div></div>",
        esc(name),
        esc(login)
    );
    h.push_str("</div>\n");
    let bio = str_of(user, "biography");
    if !bio.trim().is_empty() {
        let _ = writeln!(h, "<div class=\"bio\">{}</div>", text_to_html(bio));
    }
    let mut links: Vec<String> = vec![format!(
        "<a href=\"{SITE}/users/{0}\" target=\"_blank\" rel=\"noopener noreferrer\">Aipictors のプロフィール</a>",
        esc(&url_path(login))
    )];
    let tw = str_of(user, "twitterAccountId");
    if !tw.is_empty() {
        links.push(format!(
            "<a href=\"https://x.com/{0}\" target=\"_blank\" rel=\"noopener noreferrer\">X: @{1}</a>",
            esc(&url_path(tw)),
            esc(tw)
        ));
    }
    let site = str_of(user, "siteURL");
    if site.starts_with("http") {
        links.push(format!(
            "<a href=\"{0}\" target=\"_blank\" rel=\"noopener noreferrer\">{0}</a>",
            esc(site)
        ));
    }
    let _ = writeln!(h, "<div class=\"links\">{}</div>", links.join(" · "));
    let num = |k: &str| user.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let archived = local
        .and_then(|l| l.get("archivedAt"))
        .and_then(|v| v.as_i64())
        .map(format_time)
        .unwrap_or_default();
    let _ = writeln!(
        h,
        "<div class=\"stats\">作品 {} · フォロワー {} · フォロー {} · 受け取ったいいね {} · 閲覧 {}<br>\
         <span class=\"muted\">アーカイブ日時 {} (このアーカイブ内 {} 作品)</span></div>",
        num("worksCount"),
        num("followersCount"),
        num("followCount"),
        num("receivedLikesCount"),
        num("receivedViewsCount"),
        esc(&archived),
        works.len()
    );
    h.push_str("</header>\n");

    // フィルタ
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for w in works {
        *counts.entry(w.kind()).or_default() += 1;
    }
    h.push_str("<nav class=\"controls\">\n<div class=\"tabs\" role=\"tablist\">");
    let _ = write!(
        h,
        "<button type=\"button\" class=\"tab active\" data-type=\"\">すべて <span>{}</span></button>",
        works.len()
    );
    for k in ["WORK", "VIDEO", "COLUMN", "NOVEL"] {
        if let Some(c) = counts.get(k) {
            let _ = write!(
                h,
                "<button type=\"button\" class=\"tab\" data-type=\"{k}\">{} <span>{c}</span></button>",
                kind_label(k)
            );
        }
    }
    h.push_str("</div>\n<div class=\"filters\">");
    h.push_str("<input type=\"search\" id=\"q\" placeholder=\"タイトル・タグ・説明で検索\">");
    h.push_str("<span class=\"ratings\">");
    for r in ["G", "R15", "R18", "R18G"] {
        let _ = write!(
            h,
            "<label><input type=\"checkbox\" class=\"rating\" value=\"{r}\" checked> {r}</label>"
        );
    }
    h.push_str("</span>");
    h.push_str(
        "<select id=\"sort\"><option value=\"new\">新しい順</option><option value=\"old\">古い順</option>\
         <option value=\"likes\">いいね順</option><option value=\"views\">閲覧数順</option></select>",
    );
    let (album_list, album_map) = albums;
    if !album_list.is_empty() {
        h.push_str("<select id=\"album\"><option value=\"\">シリーズ: すべて</option>");
        for a in album_list {
            let _ = write!(
                h,
                "<option value=\"{}\">{}</option>",
                esc(&a.id),
                esc(&a.title)
            );
        }
        h.push_str("</select>");
    }
    h.push_str("<label class=\"blur\"><input type=\"checkbox\" id=\"blur\"> R18 をぼかす</label>");
    h.push_str("</div>\n<div class=\"count\" id=\"count\"></div>\n</nav>\n");

    // 作品カード
    h.push_str("<main class=\"grid\" id=\"grid\">\n");
    for w in works {
        let tags =
            w.v.get("tagNames")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
        let album_ids = album_map
            .get(&w.id)
            .map(|v| {
                v.iter()
                    .map(|i| album_list[*i].id.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let search = format!("{} {} {}", w.title(), tags, w.s("description")).to_lowercase();
        let _ = write!(
            h,
            "<a class=\"card\" href=\"works/{}/index.html\" data-type=\"{}\" data-rating=\"{}\" \
             data-date=\"{}\" data-likes=\"{}\" data-views=\"{}\" data-albums=\"{}\" data-q=\"{}\">",
            url_path(&w.dir),
            esc(w.kind()),
            esc(w.rating()),
            w.n("createdAt"),
            w.n("likesCount"),
            w.n("viewsCount"),
            esc(&album_ids),
            esc(&search)
        );
        h.push_str("<div class=\"thumb\">");
        match thumb_src(w, "") {
            Some(src) => {
                let _ = write!(h, "<img loading=\"lazy\" src=\"{src}\" alt=\"\">");
            }
            None => {
                let _ = write!(h, "<div class=\"noimg\">{}</div>", kind_label(w.kind()));
            }
        }
        h.push_str("<div class=\"badges\">");
        if w.rating() != "G" {
            let _ = write!(h, "<span class=\"badge r\">{}</span>", esc(w.rating()));
        }
        if w.kind() != "WORK" {
            let _ = write!(h, "<span class=\"badge k\">{}</span>", kind_label(w.kind()));
        }
        let n_img = w.images().len();
        if w.kind() == "WORK" && n_img > 1 {
            let _ = write!(h, "<span class=\"badge n\">{n_img}枚</span>");
        }
        h.push_str("</div></div>");
        let _ = writeln!(
            h,
            "<div class=\"meta\"><div class=\"title\">{}</div><div class=\"sub\">{} · ♥{}</div></div></a>",
            esc(w.title()),
            format_date(w.n("createdAt")),
            w.n("likesCount")
        );
    }
    h.push_str("</main>\n");
    h.push_str(
        "<footer>このページは <a href=\"https://github.com/MikageSawatari/aipicdl\" target=\"_blank\" \
         rel=\"noopener noreferrer\">aipicdl</a> で作成したローカルアーカイブです。</footer>\n",
    );
    h.push_str("<script src=\"assets/app.js\"></script>\n</body>\n</html>\n");
    h
}

fn render_markdown(md: &str, images: Option<&serde_json::Map<String, Value>>) -> String {
    use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let rewrite = |u: CowStr<'static>| -> CowStr<'static> {
        match images
            .and_then(|m| m.get(u.as_ref()))
            .and_then(|v| v.as_str())
        {
            Some(local) => CowStr::from(url_path(local)),
            None => u,
        }
    };
    // 生 HTML は表示せず文字列として出す (スクリプト等を実行させない)。
    // ただし <img src> は保存した画像に差し替えて表示する。
    let events = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::Html(s) | Event::InlineHtml(s) => {
            let urls = crate::util::markdown_image_urls(&s);
            if urls.len() == 1 && s.trim_start().to_ascii_lowercase().starts_with("<img") {
                let src = rewrite(CowStr::from(urls[0].clone()));
                Event::InlineHtml(CowStr::from(format!(
                    "<img src=\"{}\" alt=\"\">",
                    esc(&src)
                )))
            } else {
                Event::Text(s)
            }
        }
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: rewrite(CowStr::from(dest_url.into_string())),
            title,
            id,
        }),
        e => e,
    });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events);
    out
}

struct PageCtx<'a> {
    user: &'a Value,
    newer: Option<&'a Work>,
    older: Option<&'a Work>,
    stickers: &'a HashMap<String, String>,
    icons: &'a HashMap<String, String>,
    albums: &'a (Vec<Album>, HashMap<String, Vec<usize>>),
}

fn work_page(w: &Work, ctx: &PageCtx, text: Option<&str>, comments: &[Value]) -> String {
    let (newer, older, albums) = (ctx.newer, ctx.older, ctx.albums);
    let user_name = str_of(ctx.user, "name");
    let mut h = head(
        &format!("{} - {}", w.title(), user_name),
        "../../assets/style.css",
    );
    h.push_str("<body class=\"work\">\n<nav class=\"crumbs\">");
    h.push_str("<a href=\"../../index.html\">← 一覧へ</a>");
    h.push_str("<span class=\"pager\">");
    if let Some(n) = newer {
        let _ = write!(
            h,
            "<a href=\"../{}/index.html\" title=\"{}\">‹ 新しい作品</a>",
            url_path(&n.dir),
            esc(n.title())
        );
    }
    if let Some(o) = older {
        let _ = write!(
            h,
            "<a href=\"../{}/index.html\" title=\"{}\">古い作品 ›</a>",
            url_path(&o.dir),
            esc(o.title())
        );
    }
    h.push_str("</span></nav>\n<article>\n");

    let _ = writeln!(h, "<h1>{}</h1>", esc(w.title()));
    let mut meta = vec![
        format_time(w.n("createdAt")),
        kind_label(w.kind()).to_string(),
        w.rating().to_string(),
    ];
    let access = w.s("accessType");
    if !access.is_empty() && access != "PUBLIC" {
        meta.push(access.to_string());
    }
    let _ = writeln!(h, "<div class=\"meta\">{}</div>", esc(&meta.join(" · ")));

    if let Some(notes) = w.local("notes").and_then(|v| v.as_array()) {
        for n in notes.iter().filter_map(|x| x.as_str()) {
            let _ = writeln!(h, "<div class=\"note\">⚠ {}</div>", esc(n));
        }
    }

    // 本体
    let images = w.images();
    match w.kind() {
        "VIDEO" => {
            if let Some(video) = w.local_str("video") {
                let poster = images
                    .first()
                    .map(|p| format!(" poster=\"{}\"", url_path(p)))
                    .unwrap_or_default();
                if let Some(audio) = w.local_str("audio") {
                    // 映像と音声を合成できなかった場合は 2 つを同期再生する。
                    let _ = writeln!(
                        h,
                        "<div class=\"media\"><video id=\"v\" controls preload=\"metadata\"{poster} src=\"{}\"></video>\
                         <audio id=\"a\" src=\"{}\"></audio></div>\
                         <script>(function(){{var v=document.getElementById('v'),a=document.getElementById('a');\
                         v.addEventListener('play',function(){{a.currentTime=v.currentTime;a.play();}});\
                         v.addEventListener('pause',function(){{a.pause();}});\
                         v.addEventListener('seeked',function(){{a.currentTime=v.currentTime;}});\
                         v.addEventListener('volumechange',function(){{a.volume=v.volume;a.muted=v.muted;}});}})();</script>",
                        url_path(video),
                        url_path(audio)
                    );
                } else {
                    let _ = writeln!(
                        h,
                        "<div class=\"media\"><video controls preload=\"metadata\"{poster} src=\"{}\"></video></div>",
                        url_path(video)
                    );
                }
            } else {
                h.push_str("<div class=\"note\">動画ファイルは保存されていません。</div>\n");
                if let Some(p) = images.first() {
                    let _ = writeln!(
                        h,
                        "<div class=\"media\"><img src=\"{}\" alt=\"\"></div>",
                        url_path(p)
                    );
                }
            }
        }
        "COLUMN" | "NOVEL" => {
            if let Some(p) = images.first() {
                let _ = writeln!(
                    h,
                    "<div class=\"cover\"><a href=\"{0}\"><img src=\"{0}\" alt=\"\"></a></div>",
                    url_path(p)
                );
            }
        }
        _ => {
            h.push_str("<div class=\"images\">\n");
            for p in &images {
                let _ = writeln!(
                    h,
                    "<a href=\"{0}\" target=\"_blank\"><img src=\"{0}\" alt=\"\" loading=\"lazy\"></a>",
                    url_path(p)
                );
            }
            h.push_str("</div>\n");
        }
    }

    let desc = w.s("description");
    if !desc.trim().is_empty() {
        let _ = writeln!(
            h,
            "<section class=\"desc\">{}</section>",
            text_to_html(desc)
        );
    }

    if let Some(md) = text {
        let imgs = w.local("textImages").and_then(|v| v.as_object());
        let _ = writeln!(
            h,
            "<section class=\"text\">\n{}</section>",
            render_markdown(md, imgs)
        );
    }

    // タグ
    if let Some(tags) = w.v.get("tagNames").and_then(|v| v.as_array())
        && !tags.is_empty()
    {
        h.push_str("<div class=\"tags\">");
        for t in tags.iter().filter_map(|x| x.as_str()) {
            let _ = write!(
                h,
                "<a class=\"tag\" href=\"../../index.html#q={}\">#{}</a>",
                url_path(t),
                esc(t)
            );
        }
        h.push_str("</div>\n");
    }

    // 情報
    h.push_str("<table class=\"info\">\n");
    let mut row = |k: &str, v: String| {
        if !v.trim().is_empty() {
            let _ = writeln!(h, "<tr><th>{}</th><td>{}</td></tr>", esc(k), v);
        }
    };
    row(
        "反応",
        format!(
            "いいね {} · ブックマーク {} · 閲覧 {} · コメント {}",
            w.n("likesCount"),
            w.n("bookmarksCount"),
            w.n("viewsCount"),
            w.n("commentsCount")
        ),
    );
    let mut ranks = Vec::new();
    for (k, l) in [
        ("dailyRanking", "デイリー"),
        ("weeklyRanking", "ウィークリー"),
        ("monthlyRanking", "マンスリー"),
    ] {
        if let Some(r) = w.v.get(k).and_then(|x| x.as_i64()) {
            ranks.push(format!("{l} {r} 位"));
        }
    }
    row("ランキング", ranks.join(" · "));
    if let Some(t) = w.v.get("dailyTheme").filter(|x| !x.is_null()) {
        row(
            "お題",
            esc(&format!(
                "{} ({})",
                str_of(t, "title"),
                str_of(t, "dateText")
            )),
        );
    }
    let (album_list, album_map) = albums;
    if let Some(idx) = album_map.get(&w.id) {
        row(
            "シリーズ",
            idx.iter()
                .map(|i| esc(&album_list[*i].title))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    row("スタイル", esc(w.s("style")));
    row("モデル", esc(w.s("model")));
    for (k, label) in [
        ("prompt", "プロンプト"),
        ("negativePrompt", "ネガティブプロンプト"),
    ] {
        let v = w.s(k);
        if !v.trim().is_empty() {
            row(label, format!("<pre>{}</pre>", esc(v)));
        }
    }
    for (k, label) in [
        ("seed", "Seed"),
        ("steps", "Steps"),
        ("scale", "CFG Scale"),
        ("sampler", "Sampler"),
        ("vae", "VAE"),
        ("clipSkip", "Clip skip"),
        ("strength", "Strength"),
        ("noise", "Noise"),
        ("modelHash", "Model hash"),
    ] {
        let v = match w.v.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        row(label, esc(&v));
    }
    let other = w.s("otherGenerationParams");
    if !other.trim().is_empty() {
        row("その他の生成情報", format!("<pre>{}</pre>", esc(other)));
    }
    let png = w.s("pngInfo");
    if !png.trim().is_empty() {
        row(
            "PNG info",
            format!(
                "<details><summary>表示</summary><pre>{}</pre></details>",
                esc(png)
            ),
        );
    }
    let rel = w.s("relatedUrl");
    if rel.starts_with("http") {
        row("関連リンク", text_to_html(rel));
    }
    row(
        "元ページ",
        format!(
            "<a href=\"{SITE}/posts/{0}\" target=\"_blank\" rel=\"noopener noreferrer\">{SITE}/posts/{0}</a>",
            esc(&w.id)
        ),
    );
    if let Some(at) = w.local("archivedAt").and_then(|v| v.as_i64()) {
        row("保存日時", esc(&format_time(at)));
    }
    h.push_str("</table>\n");

    // コメント
    if !comments.is_empty() {
        h.push_str("<section class=\"comments\">\n<h2>コメント</h2>\n");
        for c in comments {
            comment_html(&mut h, c, ctx, false);
        }
        h.push_str("</section>\n");
    }

    h.push_str("</article>\n</body>\n</html>\n");
    h
}

fn comment_html(h: &mut String, c: &Value, ctx: &PageCtx, is_reply: bool) {
    let user = c.get("user").filter(|u| !u.is_null());
    let name = user.map(|u| str_of(u, "name")).unwrap_or("");
    let login = user.map(|u| str_of(u, "login")).unwrap_or("");
    let uid = user
        .and_then(|u| u.get("id"))
        .and_then(crate::api::id_string);
    let _ = write!(
        h,
        "<div class=\"comment{}\">",
        if is_reply { " reply" } else { "" }
    );
    match uid
        .as_ref()
        .and_then(|id| ctx.icons.get(&crate::util::safe_file_name(id)))
    {
        Some(f) => {
            let _ = write!(
                h,
                "<img class=\"cicon\" src=\"../../icons/{}\" alt=\"\">",
                url_path(f)
            );
        }
        None => h.push_str("<span class=\"cicon\"></span>"),
    }
    h.push_str("<div class=\"cbody\">");
    let who = if name.is_empty() {
        "(名前なし)"
    } else {
        name
    };
    let login_html = if login.is_empty() {
        String::new()
    } else {
        format!(" <span class=\"clogin\">@{}</span>", esc(login))
    };
    let _ = write!(
        h,
        "<div class=\"chead\"><span class=\"cname\">{}</span>{login_html}<span class=\"cdate\">{}</span></div>",
        esc(who),
        format_time(c.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0))
    );
    let deleted = c
        .get("isDeleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let text = str_of(c, "text");
    if deleted {
        h.push_str("<div class=\"ctext muted\">(削除されたコメント)</div>");
    } else if !text.trim().is_empty() {
        let _ = write!(h, "<div class=\"ctext\">{}</div>", text_to_html(text));
    }
    if let Some(st) = c.get("sticker").filter(|s| !s.is_null()) {
        let sid = st
            .get("id")
            .and_then(crate::api::id_string)
            .unwrap_or_default();
        if let Some(f) = ctx.stickers.get(&crate::util::safe_file_name(&sid)) {
            let t = esc(str_of(st, "title"));
            let _ = write!(
                h,
                "<img class=\"sticker\" src=\"../../stickers/{}\" alt=\"{t}\" title=\"{t}\">",
                url_path(f)
            );
        }
    }
    if let Some(rs) = c.get("responses").and_then(|v| v.as_array()) {
        for r in rs {
            comment_html(h, r, ctx, true);
        }
    }
    h.push_str("</div></div>\n");
}
