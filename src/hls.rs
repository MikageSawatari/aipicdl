//! Cloudflare Stream の HLS (fMP4) を 1 本の MP4 にまとめる。
//!
//! 動画と音声は別々の rendition (それぞれ init.mp4 + seg_N.mp4) で配信されるため、
//! 両方の init から moov を合成し、moof/mdat を時刻順に並べ直した fragmented MP4 を作る。
//! 再エンコードはしないので画質・音質は配信データそのまま。

use anyhow::{Context, Result, anyhow, bail};

// ---------------------------------------------------------------------------
// m3u8
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub height: u64,
    pub audio_group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AudioMedia {
    pub group_id: String,
    pub uri: String,
    pub default: bool,
}

#[derive(Debug, Default)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub audios: Vec<AudioMedia>,
}

#[derive(Debug, Default)]
pub struct MediaPlaylist {
    pub init: Option<String>,
    pub segments: Vec<String>,
    pub duration: f64,
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        rest = &rest[eq + 1..];
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            value = r[..end].to_string();
            rest = &r[(end + 1).min(r.len())..];
            rest = rest.strip_prefix(',').unwrap_or(rest);
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].to_string();
            rest = &rest[(end + 1).min(rest.len())..];
        }
        out.push((key, value));
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

pub fn parse_master(text: &str) -> Master {
    let mut m = Master::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in text.lines().map(str::trim) {
        if let Some(a) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attrs(a);
            if attr(&attrs, "TYPE") == Some("AUDIO")
                && let Some(uri) = attr(&attrs, "URI")
            {
                m.audios.push(AudioMedia {
                    group_id: attr(&attrs, "GROUP-ID").unwrap_or("").to_string(),
                    uri: uri.to_string(),
                    default: attr(&attrs, "DEFAULT") == Some("YES"),
                });
            }
        } else if let Some(a) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attrs(a));
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some(attrs) = pending.take()
        {
            let height = attr(&attrs, "RESOLUTION")
                .and_then(|r| r.split('x').nth(1))
                .and_then(|h| h.parse().ok())
                .unwrap_or(0);
            m.variants.push(Variant {
                uri: line.to_string(),
                bandwidth: attr(&attrs, "BANDWIDTH")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0),
                height,
                audio_group: attr(&attrs, "AUDIO").map(|s| s.to_string()),
            });
        }
    }
    m
}

pub fn parse_media(text: &str) -> MediaPlaylist {
    let mut p = MediaPlaylist::default();
    for line in text.lines().map(str::trim) {
        if let Some(a) = line.strip_prefix("#EXT-X-MAP:") {
            p.init = attr(&parse_attrs(a), "URI").map(|s| s.to_string());
        } else if let Some(d) = line.strip_prefix("#EXTINF:") {
            p.duration += d
                .split(',')
                .next()
                .and_then(|x| x.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
        } else if !line.is_empty() && !line.starts_with('#') {
            p.segments.push(line.to_string());
        }
    }
    p
}

impl Master {
    /// 最も解像度 (同率ならビットレート) の高い映像と、それに対応する音声を選ぶ。
    pub fn best(&self) -> Option<(&Variant, Option<&AudioMedia>)> {
        let v = self
            .variants
            .iter()
            .max_by_key(|v| (v.height, v.bandwidth))?;
        let audio = match &v.audio_group {
            Some(g) => {
                let in_group: Vec<&AudioMedia> =
                    self.audios.iter().filter(|a| &a.group_id == g).collect();
                in_group
                    .iter()
                    .find(|a| a.default)
                    .or(in_group.first())
                    .copied()
            }
            None => None,
        };
        Some((v, audio))
    }
}

// ---------------------------------------------------------------------------
// ISO BMFF
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Bx {
    typ: [u8; 4],
    start: usize,
    body: usize,
    end: usize,
}

fn boxes(data: &[u8], from: usize, to: usize) -> Result<Vec<Bx>> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos + 8 <= to {
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into()?) as usize;
        let typ: [u8; 4] = data[pos + 4..pos + 8].try_into()?;
        let (size, header) = match size32 {
            0 => (to - pos, 8),
            1 => {
                if pos + 16 > to {
                    bail!("box header truncated");
                }
                (
                    u64::from_be_bytes(data[pos + 8..pos + 16].try_into()?) as usize,
                    16,
                )
            }
            n => (n, 8),
        };
        if size < header || pos + size > to {
            bail!("box '{}' のサイズが不正です", String::from_utf8_lossy(&typ));
        }
        out.push(Bx {
            typ,
            start: pos,
            body: pos + header,
            end: pos + size,
        });
        pos += size;
    }
    Ok(out)
}

fn find(list: &[Bx], typ: &[u8; 4]) -> Option<Bx> {
    list.iter().find(|b| &b.typ == typ).copied()
}

fn child(data: &[u8], parent: Bx, typ: &[u8; 4]) -> Result<Option<Bx>> {
    Ok(find(&boxes(data, parent.body, parent.end)?, typ))
}

fn path(data: &[u8], parent: Bx, types: &[&[u8; 4]]) -> Result<Option<Bx>> {
    let mut cur = parent;
    for t in types {
        match child(data, cur, t)? {
            Some(b) => cur = b,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}
fn be64(d: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(d[at..at + 8].try_into().unwrap())
}
fn put32(d: &mut [u8], at: usize, v: u32) {
    d[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64(d: &mut [u8], at: usize, v: u64) {
    d[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

fn make_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 8);
    v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

/// 1 本の rendition (init + segments) を解析した結果。
struct Track {
    init: Vec<u8>,
    ftyp: Option<Bx>,
    mvhd: Bx,
    trak: Bx,
    trex: Option<Bx>,
    moov_others: Vec<Bx>,
    movie_timescale: u32,
    fragments: Vec<Fragment>,
}

struct Fragment {
    /// moof に続く mdat までを含むバイト列。
    bytes: Vec<u8>,
    /// `bytes` 内の moof の範囲。
    moof_len: usize,
    time: f64,
}

fn parse_track(init: Vec<u8>, segments: &[Vec<u8>]) -> Result<Track> {
    let top = boxes(&init, 0, init.len())?;
    let moov = find(&top, b"moov").ok_or_else(|| anyhow!("init に moov がありません"))?;
    let moov_children = boxes(&init, moov.body, moov.end)?;
    let mvhd = find(&moov_children, b"mvhd").ok_or_else(|| anyhow!("mvhd がありません"))?;
    let traks: Vec<Bx> = moov_children
        .iter()
        .filter(|b| &b.typ == b"trak")
        .copied()
        .collect();
    if traks.len() != 1 {
        bail!("トラック数が 1 ではありません ({})", traks.len());
    }
    let trak = traks[0];
    let trex = match find(&moov_children, b"mvex") {
        Some(mvex) => child(&init, mvex, b"trex")?,
        None => None,
    };
    let moov_others = moov_children
        .iter()
        .filter(|b| !matches!(&b.typ, b"mvhd" | b"trak" | b"mvex"))
        .copied()
        .collect();

    let mvhd_v = init[mvhd.body];
    let movie_timescale = be32(&init, mvhd.body + 4 + if mvhd_v == 1 { 16 } else { 8 });
    let mdhd =
        path(&init, trak, &[b"mdia", b"mdhd"])?.ok_or_else(|| anyhow!("mdhd がありません"))?;
    let mdhd_v = init[mdhd.body];
    let media_timescale = be32(&init, mdhd.body + 4 + if mdhd_v == 1 { 16 } else { 8 });
    if media_timescale == 0 || movie_timescale == 0 {
        bail!("timescale が 0 です");
    }

    let mut fragments = Vec::new();
    for seg in segments {
        let mut current: Option<Fragment> = None;
        for b in boxes(seg, 0, seg.len())? {
            match &b.typ {
                b"moof" => {
                    if let Some(f) = current.take() {
                        fragments.push(f);
                    }
                    let moof = &seg[b.start..b.end];
                    let time = moof_decode_time(moof)? as f64 / media_timescale as f64;
                    current = Some(Fragment {
                        bytes: moof.to_vec(),
                        moof_len: moof.len(),
                        time,
                    });
                }
                b"mdat" => match current.as_mut() {
                    Some(f) => f.bytes.extend_from_slice(&seg[b.start..b.end]),
                    None => bail!("moof より前に mdat があります"),
                },
                // styp / sidx / prft などは位置情報が変わるため捨てる。
                _ => {}
            }
        }
        if let Some(f) = current.take() {
            fragments.push(f);
        }
    }
    if fragments.is_empty() {
        bail!("フラグメントがありません");
    }

    Ok(Track {
        ftyp: find(&top, b"ftyp"),
        init,
        mvhd,
        trak,
        trex,
        moov_others,
        movie_timescale,
        fragments,
    })
}

fn moof_decode_time(moof: &[u8]) -> Result<u64> {
    let top = boxes(moof, 0, moof.len())?;
    let m = top[0];
    let Some(traf) = child(moof, m, b"traf")? else {
        bail!("traf がありません")
    };
    match child(moof, traf, b"tfdt")? {
        Some(t) => Ok(if moof[t.body] == 1 {
            be64(moof, t.body + 4)
        } else {
            be32(moof, t.body + 4) as u64
        }),
        None => bail!("tfdt がありません"),
    }
}

/// moof 内の mfhd sequence_number と tfhd track_ID を書き換える。
fn patch_moof(frag: &mut Fragment, seq: u32, track_id: u32) -> Result<()> {
    let moof_len = frag.moof_len;
    let d = &mut frag.bytes[..moof_len];
    let m = boxes(d, 0, moof_len)?[0];
    for b in boxes(d, m.body, m.end)? {
        match &b.typ {
            b"mfhd" => put32(d, b.body + 4, seq),
            b"traf" => {
                let tfhd = child(d, b, b"tfhd")?.ok_or_else(|| anyhow!("tfhd がありません"))?;
                let flags = be32(d, tfhd.body) & 0x00ff_ffff;
                if flags & 0x01 != 0 {
                    // 絶対オフセット指定は並べ替えると壊れるので対応しない (CMAF では使われない)。
                    bail!("base-data-offset 付きの tfhd には未対応です");
                }
                put32(d, tfhd.body + 4, track_id);
            }
            _ => {}
        }
    }
    Ok(())
}

fn patch_tkhd(trak: &mut [u8], track_id: u32, duration: u64) -> Result<()> {
    let list = boxes(trak, 8, trak.len())?;
    let tkhd = find(&list, b"tkhd").ok_or_else(|| anyhow!("tkhd がありません"))?;
    let v = trak[tkhd.body];
    let p = tkhd.body + 4;
    if v == 1 {
        put32(trak, p + 16, track_id);
        put64(trak, p + 24, duration);
    } else {
        put32(trak, p + 8, track_id);
        put32(trak, p + 16, duration.min(u32::MAX as u64) as u32);
    }
    Ok(())
}

/// edts/elst の segment_duration は movie timescale 単位なので、合成先の timescale に直す。
fn rescale_elst(trak: &mut [u8], from_ts: u32, to_ts: u32) -> Result<()> {
    if from_ts == to_ts {
        return Ok(());
    }
    let list = boxes(trak, 8, trak.len())?;
    let Some(edts) = find(&list, b"edts") else {
        return Ok(());
    };
    let Some(elst) = find(&boxes(trak, edts.body, edts.end)?, b"elst") else {
        return Ok(());
    };
    let v = trak[elst.body];
    let count = be32(trak, elst.body + 4) as usize;
    let mut p = elst.body + 8;
    let entry = if v == 1 { 20 } else { 12 };
    for _ in 0..count {
        if p + entry > elst.end {
            break;
        }
        let scale = |x: u64| (x as u128 * to_ts as u128 / from_ts as u128) as u64;
        if v == 1 {
            let d = be64(trak, p);
            put64(trak, p, scale(d));
        } else {
            let d = be32(trak, p) as u64;
            put32(trak, p, scale(d).min(u32::MAX as u64) as u32);
        }
        p += entry;
    }
    Ok(())
}

fn trex_with_id(t: &Track, track_id: u32) -> Vec<u8> {
    match t.trex {
        Some(b) => {
            let mut v = t.init[b.start..b.end].to_vec();
            put32(&mut v, b.body - b.start + 4, track_id);
            v
        }
        None => {
            // 既定値だけの trex を作る。
            let mut p = vec![0u8; 4];
            p.extend_from_slice(&track_id.to_be_bytes());
            p.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
            p.extend_from_slice(&[0u8; 12]);
            make_box(b"trex", &p)
        }
    }
}

/// 映像と音声の rendition を 1 本の fragmented MP4 にまとめる。
pub fn mux(
    video_init: Vec<u8>,
    video_segments: &[Vec<u8>],
    audio_init: Vec<u8>,
    audio_segments: &[Vec<u8>],
    duration_secs: f64,
) -> Result<Vec<u8>> {
    let mut v = parse_track(video_init, video_segments).context("映像トラックの解析")?;
    let mut a = parse_track(audio_init, audio_segments).context("音声トラックの解析")?;
    let ts = v.movie_timescale;
    let movie_duration = (duration_secs.max(0.0) * ts as f64).round() as u64;

    // moov
    let mut mvhd = v.init[v.mvhd.start..v.mvhd.end].to_vec();
    {
        let body = v.mvhd.body - v.mvhd.start;
        let ver = mvhd[body];
        let p = body + 4;
        if ver == 1 {
            put64(&mut mvhd, p + 20, movie_duration);
        } else {
            put32(
                &mut mvhd,
                p + 12,
                movie_duration.min(u32::MAX as u64) as u32,
            );
        }
        let len = mvhd.len();
        put32(&mut mvhd, len - 4, 3); // next_track_ID
    }
    let mut vtrak = v.init[v.trak.start..v.trak.end].to_vec();
    patch_tkhd(&mut vtrak, 1, movie_duration)?;
    let mut atrak = a.init[a.trak.start..a.trak.end].to_vec();
    patch_tkhd(&mut atrak, 2, movie_duration)?;
    rescale_elst(&mut atrak, a.movie_timescale, ts)?;

    let mut mvex = Vec::new();
    {
        let mut mehd = vec![1u8, 0, 0, 0];
        mehd.extend_from_slice(&movie_duration.to_be_bytes());
        mvex.extend(make_box(b"mehd", &mehd));
        mvex.extend(trex_with_id(&v, 1));
        mvex.extend(trex_with_id(&a, 2));
    }
    let mut moov = Vec::new();
    moov.extend(mvhd);
    moov.extend(vtrak);
    moov.extend(atrak);
    moov.extend(make_box(b"mvex", &mvex));
    for b in &v.moov_others {
        moov.extend_from_slice(&v.init[b.start..b.end]);
    }

    let mut out = Vec::new();
    match v.ftyp {
        Some(f) => out.extend_from_slice(&v.init[f.start..f.end]),
        None => out.extend(make_box(b"ftyp", b"iso6\0\0\0\0iso6mp41")),
    }
    out.extend(make_box(b"moov", &moov));

    // 映像・音声のフラグメントを時刻順に交互に並べる。
    let mut vi = std::mem::take(&mut v.fragments).into_iter().peekable();
    let mut ai = std::mem::take(&mut a.fragments).into_iter().peekable();
    let mut seq = 1u32;
    loop {
        let take_video = match (vi.peek(), ai.peek()) {
            (Some(x), Some(y)) => x.time <= y.time,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        let (mut f, id) = if take_video {
            (vi.next().unwrap(), 1)
        } else {
            (ai.next().unwrap(), 2)
        };
        patch_moof(&mut f, seq, id)?;
        seq += 1;
        out.extend_from_slice(&f.bytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_master_and_picks_best() {
        let m = parse_master(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"group_audio\",NAME=\"original\",DEFAULT=YES,URI=\"a.m3u8\"\n\
             #EXT-X-STREAM-INF:RESOLUTION=1280x720,CODECS=\"avc1.4d4020,mp4a.40.2\",BANDWIDTH=3221030,AUDIO=\"group_audio\"\n\
             v720.m3u8\n\
             #EXT-X-STREAM-INF:RESOLUTION=1920x1080,CODECS=\"avc1.4d402a,mp4a.40.2\",BANDWIDTH=6007356,AUDIO=\"group_audio\"\n\
             v1080.m3u8\n",
        );
        let (v, a) = m.best().unwrap();
        assert_eq!(v.uri, "v1080.m3u8");
        assert_eq!(a.unwrap().uri, "a.m3u8");
    }

    #[test]
    fn parses_media_playlist() {
        let p = parse_media(
            "#EXTM3U\n#EXT-X-MAP:URI=\"../init.mp4?p=a,b\"\n#EXTINF:4.004,\nseg_1.mp4?x\n#EXTINF:2.5,\nseg_2.mp4\n#EXT-X-ENDLIST\n",
        );
        assert_eq!(p.init.as_deref(), Some("../init.mp4?p=a,b"));
        assert_eq!(p.segments, vec!["seg_1.mp4?x", "seg_2.mp4"]);
        assert!((p.duration - 6.504).abs() < 1e-9);
    }
}
