//! Aipictors の GraphQL API (https://api.aipictors.com/) に投げるクエリ。
//! ログイン不要で見える公開データだけを取得する。

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::net::Client;

/// `ratings` を明示しないと R18 / R18G 作品が一覧から除外されるため、全レーティングを指定する。
const ALL_RATINGS: [&str; 4] = ["G", "R15", "R18", "R18G"];

/// 一覧取得の 1 ページあたりの件数。1 回のレスポンスを軽く保つため控えめにしている。
pub const LIST_PAGE: usize = 50;
const COMMENT_PAGE: usize = 50;

const USER_FIELDS: &str = "id login name nanoid biography enBiography iconUrl headerImageUrl \
    siteURL twitterAccountId instagramAccountId githubAccountId createdAt worksCount \
    followersCount followCount receivedLikesCount receivedViewsCount albumCount";

const WORK_FIELDS: &str = "id uuid nanoid type title enTitle description enDescription \
    createdAt updatedAt accessType rating isSensitive isDeleted style \
    imageURL imageWidth imageHeight largeThumbnailImageURL smallThumbnailImageURL \
    subWorksCount subWorks { id imageUrl } \
    mdUrl url streamUid relatedUrl \
    isGeneration generationModelId promptAccessType prompt negativePrompt seed steps scale \
    sampler model modelHash vae clipSkip strength noise pngInfo otherGenerationParams \
    tagNames likesCount bookmarksCount viewsCount commentsCount \
    dailyRanking weeklyRanking monthlyRanking \
    user { id login name } album { id title slug } \
    dailyTheme { id title dateText }";

const COMMENT_FIELDS: &str = "id text createdAt isDeleted isSensitive likesCount \
    user { id login name iconUrl } sticker { id title imageUrl }";

/// プロフィール URL (またはユーザー ID) から `user(id:)` に渡す値を取り出す。
pub fn parse_user_input(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    if let Ok(u) = url::Url::parse(s) {
        let segs: Vec<&str> = u.path_segments()?.filter(|x| !x.is_empty()).collect();
        let pos = segs.iter().position(|x| *x == "users")?;
        let id = segs.get(pos + 1)?;
        return Some(percent_decode(id));
    }
    if let Some(rest) = s.split("users/").nth(1) {
        return rest.split(['/', '?', '#']).next().map(percent_decode);
    }
    if s.contains('/') || s.contains(' ') {
        return None;
    }
    Some(s.to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn fetch_user(client: &Client, login: &str) -> Result<Value> {
    let q = format!(
        "query User($id: ID!) {{ user(id: $id) {{ {USER_FIELDS} \
         albums(offset: 0, limit: 100) {{ id slug title description createdAt worksCount workIds thumbnailImageURL }} }} }}"
    );
    let data = client.graphql("ユーザー情報", &q, json!({ "id": login }))?;
    let user = data.get("user").cloned().unwrap_or(Value::Null);
    if user.is_null() {
        return Err(anyhow!("ユーザー「{login}」が見つかりませんでした"));
    }
    Ok(user)
}

fn works_where(user_id: &str) -> Value {
    json!({
        "userId": user_id,
        "ratings": ALL_RATINGS,
        "orderBy": "DATE_CREATED",
        "sort": "DESC",
    })
}

pub fn fetch_works_count(client: &Client, user_id: &str) -> Result<usize> {
    let q = "query WorksCount($where: WorksWhereInput) { worksCount(where: $where) }";
    let data = client.graphql("作品数", q, json!({ "where": works_where(user_id) }))?;
    Ok(data.get("worksCount").and_then(|v| v.as_u64()).unwrap_or(0) as usize)
}

/// 一覧は ID と作成日時だけ取り、詳細は作品ごとに `fetch_work` で取る (サイトの作品ページと同じ粒度)。
pub fn fetch_work_ids_page(client: &Client, user_id: &str, offset: usize) -> Result<Vec<String>> {
    let q = "query UserWorkIds($offset: Int!, $limit: Int!, $where: WorksWhereInput) { \
             works(offset: $offset, limit: $limit, where: $where) { id createdAt } }";
    let data = client.graphql(
        &format!("作品一覧 ({offset}〜)"),
        q,
        json!({ "offset": offset, "limit": LIST_PAGE, "where": works_where(user_id) }),
    )?;
    let ids = data
        .get("works")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|w| w.get("id").and_then(id_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ids)
}

pub fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn comments_selection() -> String {
    format!(
        "comments(offset: $offset, limit: $limit) {{ {COMMENT_FIELDS} \
         responses(offset: 0, limit: 100) {{ {COMMENT_FIELDS} }} }}"
    )
}

/// 作品の詳細を取得する。`with_comments` のときはコメントの 1 ページ目も同じリクエストで取り、
/// 足りなければ続きを取得して `comments` フィールドに全件入れて返す。
pub fn fetch_work(client: &Client, id: &str, with_comments: bool) -> Result<Value> {
    let q = if with_comments {
        format!(
            "query Work($id: ID!, $offset: Int!, $limit: Int!) {{ work(id: $id) {{ {WORK_FIELDS} {} }} }}",
            comments_selection()
        )
    } else {
        format!("query Work($id: ID!) {{ work(id: $id) {{ {WORK_FIELDS} }} }}")
    };
    let vars = if with_comments {
        json!({ "id": id, "offset": 0, "limit": COMMENT_PAGE })
    } else {
        json!({ "id": id })
    };
    let data = client.graphql(&format!("作品 {id}"), &q, vars)?;
    let mut work = data.get("work").cloned().unwrap_or(Value::Null);
    if work.is_null() {
        return Err(anyhow!("作品 {id} が取得できませんでした"));
    }
    if with_comments {
        let mut all: Vec<Value> = work
            .get("comments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if all.len() >= COMMENT_PAGE {
            all.extend(fetch_more_comments(client, id, all.len())?);
        }
        work["comments"] = Value::Array(all);
    }
    Ok(work)
}

/// `offset` 件目以降のコメント (返信込み) を取得する。
fn fetch_more_comments(client: &Client, id: &str, mut offset: usize) -> Result<Vec<Value>> {
    let q = format!(
        "query WorkComments($id: ID!, $offset: Int!, $limit: Int!) {{ work(id: $id) {{ {} }} }}",
        comments_selection()
    );
    let mut all = Vec::new();
    // 異常なレスポンスで無限ループしないよう上限を設ける。
    for _ in 0..200 {
        let data = client.graphql(
            &format!("作品 {id} のコメント"),
            &q,
            json!({ "id": id, "offset": offset, "limit": COMMENT_PAGE }),
        )?;
        let page: Vec<Value> = data
            .pointer("/work/comments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let n = page.len();
        all.extend(page);
        offset += n;
        if n < COMMENT_PAGE {
            break;
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::parse_user_input;

    #[test]
    fn parses_profile_urls() {
        let p = |s| parse_user_input(s);
        assert_eq!(
            p("https://www.aipictors.com/users/mikage").as_deref(),
            Some("mikage")
        );
        assert_eq!(
            p("https://www.aipictors.com/users/mikage/").as_deref(),
            Some("mikage")
        );
        assert_eq!(
            p("https://www.aipictors.com/en/users/mikage/posts").as_deref(),
            Some("mikage")
        );
        assert_eq!(
            p("https://www.aipictors.com/r/users/mikage?x=1").as_deref(),
            Some("mikage")
        );
        assert_eq!(
            p("www.aipictors.com/users/foo_bar").as_deref(),
            Some("foo_bar")
        );
        assert_eq!(
            p("https://www.aipictors.com/users/a%40b").as_deref(),
            Some("a@b")
        );
        assert_eq!(p("mikage").as_deref(), Some("mikage"));
        assert_eq!(p("  ").as_deref(), None);
        assert_eq!(p("https://www.aipictors.com/posts/123").as_deref(), None);
    }
}
