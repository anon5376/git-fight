//! Per-repo player stats, HTML leaderboard, and shields-style SVG badges.

use crate::db::{self, PlayerStat};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sqlx::SqlitePool;
use std::collections::BTreeMap;

pub fn is_github_segment(s: &str) -> bool {
    crate::gh::is_safe_github_name(s)
}

pub async fn record_round(
    pool: &SqlitePool,
    match_id: &str,
    round: i64,
    winner: &str,
    ko: bool,
) -> Result<(), sqlx::Error> {
    let Some(row) = db::get_match(pool, match_id).await? else {
        return Ok(());
    };
    if row.owner.is_empty() || row.repo.is_empty() {
        return Ok(());
    }
    if !matches!(row.status.as_str(), "pending" | "in_progress") {
        return Ok(());
    }
    let hunks = db::list_hunks(pool, match_id).await?;
    let theirs_login = hunks
        .iter()
        .find(|h| h.round_index == round)
        .and_then(|h| h.theirs_login.clone())
        .or(row.theirs_login);
    let ours_login = row.ours_login;

    let ours_win = matches!(winner, "ours" | "forfeit_theirs");
    let theirs_win = matches!(winner, "theirs" | "forfeit_ours");
    let ko_n = i64::from(ko && (ours_win || theirs_win));

    let mut delta: BTreeMap<String, [i64; 4]> = BTreeMap::new();
    let bump = |map: &mut BTreeMap<String, [i64; 4]>, login: Option<&str>, d: [i64; 4]| {
        let Some(login) = login
            .and_then(crate::gh::normalize_github_login)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let slot = map.entry(login).or_insert([0, 0, 0, 0]);
        for i in 0..4 {
            slot[i] += d[i];
        }
    };
    // [wins, losses, kos, conflicts_caused]
    if ours_win {
        bump(&mut delta, ours_login.as_deref(), [1, 0, ko_n, 0]);
        bump(&mut delta, theirs_login.as_deref(), [0, 1, 0, 0]);
    } else if theirs_win {
        bump(&mut delta, theirs_login.as_deref(), [1, 0, ko_n, 0]);
        bump(&mut delta, ours_login.as_deref(), [0, 1, 0, 0]);
    }
    bump(&mut delta, theirs_login.as_deref(), [0, 0, 0, 1]);

    for (login, [wins, losses, kos, caused]) in delta {
        if wins == 0 && losses == 0 && kos == 0 && caused == 0 {
            continue;
        }
        db::add_player_stats(
            pool,
            &row.owner,
            &row.repo,
            &PlayerStat {
                github_login: login,
                wins,
                losses,
                kos,
                conflicts_caused: caused,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn leaderboard_response(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    headers: &HeaderMap,
) -> Response {
    if !is_github_segment(owner) || !is_github_segment(repo) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let rows = match db::list_player_stats(pool, owner, repo).await {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let want_json = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| {
            a.split(',')
                .any(|p| p.trim().starts_with("application/json"))
        });
    if want_json {
        let body = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "players": rows.iter().map(|p| serde_json::json!({
                "login": p.github_login,
                "wins": p.wins,
                "losses": p.losses,
                "kos": p.kos,
                "conflicts_caused": p.conflicts_caused,
            })).collect::<Vec<_>>(),
        });
        return (
            [(CONTENT_TYPE, "application/json; charset=utf-8")],
            body.to_string(),
        )
            .into_response();
    }
    (
        [(CONTENT_TYPE, "text/html; charset=utf-8")],
        leaderboard_html(owner, repo, &rows),
    )
        .into_response()
}

pub async fn badge_response(pool: &SqlitePool, owner: &str, repo: &str, user: &str) -> Response {
    if !is_github_segment(owner) || !is_github_segment(repo) || !is_github_segment(user) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let stat = match db::get_player_stats(pool, owner, repo, user).await {
        Ok(s) => s,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let svg = shields_svg("git fight", &format!("{} wins", stat.wins));
    (
        [
            (CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (CACHE_CONTROL, "public, max-age=60"),
        ],
        svg,
    )
        .into_response()
}

fn leaderboard_html(owner: &str, repo: &str, rows: &[PlayerStat]) -> String {
    let mut body = String::new();
    if rows.is_empty() {
        body.push_str("<p class=\"empty\">no fights yet</p>");
    } else {
        body.push_str(
            "<table><thead><tr><th>#</th><th>fighter</th><th>wins</th><th>losses</th><th>KOs</th><th>conflicts caused</th></tr></thead><tbody>",
        );
        for (i, p) in rows.iter().enumerate() {
            body.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                i + 1,
                esc(&p.github_login),
                p.wins,
                p.losses,
                p.kos,
                p.conflicts_caused
            ));
        }
        body.push_str("</tbody></table>");
    }
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="UTF-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>git fight · {owner}/{repo}</title>
<style>
html,body{{margin:0;min-height:100%;background:#0A0A0B;color:#EEEEEA;font-family:ui-monospace,"Cascadia Code","SF Mono",Menlo,Consolas,monospace}}
main{{width:min(920px,100vw - 24px);margin:2rem auto}}
h1{{font-size:1.1rem;letter-spacing:.12em;text-transform:uppercase;font-weight:normal}}
.tag{{opacity:.8;font-size:13px}}
a{{color:#FF4A1C}}
table{{width:100%;border-collapse:collapse;margin-top:1.5rem}}
th,td{{text-align:left;padding:.55rem .4rem;border-bottom:1px solid #222}}
th{{color:#FF4A1C;font-weight:normal;letter-spacing:.08em;text-transform:uppercase;font-size:12px}}
.empty{{opacity:.7;margin-top:2rem}}
</style>
</head>
<body>
<main>
<h1>git fight</h1>
<p class="tag">{owner}/{repo} leaderboard</p>
{body}
</main>
</body>
</html>
"#,
        owner = esc(owner),
        repo = esc(repo),
        body = body
    )
}

pub fn shields_svg(label: &str, message: &str) -> String {
    let left_w = badge_width(label);
    let right_w = badge_width(message);
    let width = left_w + right_w;
    let label_e = esc(label);
    let message_e = esc(message);
    let left_cx = left_w * 5;
    let right_cx = (left_w + right_w / 2) * 10;
    let label_len = (label.chars().count() as i32 * 70).max(1);
    let message_len = (message.chars().count() as i32 * 70).max(1);
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="20" role="img" aria-label="{label_e}: {message_e}">
<title>{label_e}: {message_e}</title>
<linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
<clipPath id="r"><rect width="{width}" height="20" rx="3" fill="#fff"/></clipPath>
<g clip-path="url(#r)">
<rect width="{left_w}" height="20" fill="#0A0A0B"/>
<rect x="{left_w}" width="{right_w}" height="20" fill="#FF4A1C"/>
<rect width="{width}" height="20" fill="url(#s)"/>
</g>
<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" text-rendering="geometricPrecision" font-size="110">
<text aria-hidden="true" x="{left_cx}" y="150" fill="#010101" fill-opacity=".3" transform="scale(.1)" textLength="{label_len}">{label_e}</text>
<text x="{left_cx}" y="140" transform="scale(.1)" fill="#EEEEEA" textLength="{label_len}">{label_e}</text>
<text aria-hidden="true" x="{right_cx}" y="150" fill="#010101" fill-opacity=".3" transform="scale(.1)" textLength="{message_len}">{message_e}</text>
<text x="{right_cx}" y="140" transform="scale(.1)" fill="#fff" textLength="{message_len}">{message_e}</text>
</g>
</svg>
"##
    )
}

fn badge_width(s: &str) -> i32 {
    10 + 6 * s.chars().count() as i32 + 10
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_reject_path_tricks() {
        assert!(is_github_segment("acme"));
        assert!(is_github_segment("git-fight"));
        assert!(!is_github_segment(""));
        assert!(!is_github_segment("../main"));
        assert!(!is_github_segment("a/b"));
        assert!(!is_github_segment("a b"));
        assert!(
            is_github_segment(&"r".repeat(100)),
            "GitHub repos are up to 100"
        );
        assert!(!is_github_segment(&"r".repeat(101)));
    }

    #[test]
    fn shields_uses_palette_and_escapes() {
        let svg = shields_svg("git fight", "3 wins");
        assert!(svg.contains("fill=\"#0A0A0B\""));
        assert!(svg.contains("fill=\"#FF4A1C\""));
        assert!(svg.contains("3 wins"));
        let dirty = shields_svg("x", "<script>");
        assert!(dirty.contains("&lt;script&gt;"));
        assert!(!dirty.contains("<script>"));
    }
}
