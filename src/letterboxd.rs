use std::{
    collections::HashSet,
    env,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, StatusCode};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

const API_BASE: &str = "https://api.letterboxd.com/api/v0";
const WEBSITE: &str = "https://letterboxd.com";
const USER_AGENT: &str =
    "letterboxd-list-syncer/0.1 (+https://github.com/shadrunov/letterboxd-list-syncer)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Film {
    pub lid: String,
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct SourceList {
    pub description: String,
    pub films: Vec<Film>,
}

#[derive(Debug, Clone)]
pub struct TargetList {
    pub lid: String,
    pub version: u64,
}

#[derive(Clone)]
pub struct LetterboxdClient {
    http: Client,
    user_cookie: Option<String>,
    csrf: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateList<'a> {
    published: bool,
    name: &'a str,
    share_policy: &'a str,
    ranked: bool,
    description: &'a str,
    tags: Vec<String>,
    entries: Vec<ListUpdateEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateList<'a> {
    version: u64,
    published: bool,
    name: &'a str,
    share_policy: &'a str,
    ranked: bool,
    description: &'a str,
    tags: Vec<String>,
    entries: Vec<ListUpdateEntry>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "action")]
enum ListUpdateEntry {
    #[serde(rename = "CLEAR")]
    Clear,
    #[serde(rename = "ADD")]
    Add { listable: String },
}

#[derive(Debug, Deserialize)]
struct SaveResponse {
    #[serde(default)]
    messages: Vec<ApiMessage>,
    data: Option<SavedList>,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct SavedList {
    id: String,
    name: String,
    version: Option<u64>,
}

impl LetterboxdClient {
    pub fn from_env() -> Result<Self> {
        let user_cookie = env::var("LETTERBOXD_USER_COOKIE").ok();
        let csrf = env::var("LETTERBOXD_CSRF_COOKIE").ok();
        match (&user_cookie, &csrf) {
            (Some(_), Some(_)) | (None, None) => {}
            _ => bail!("LETTERBOXD_USER_COOKIE and LETTERBOXD_CSRF_COOKIE must be set together"),
        }

        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            http,
            user_cookie,
            csrf,
        })
    }

    pub async fn fetch_public_list(&self, url: &Url) -> Result<SourceList> {
        let mut films = Vec::new();
        let mut seen = HashSet::new();
        let mut description = String::new();
        let mut page = 1_u32;

        loop {
            let page_url = if page == 1 {
                url.clone()
            } else {
                url.join(&format!("page/{page}/"))?
            };
            let response = self.http.get(page_url.clone()).send().await?;
            if page > 1 && response.status() == StatusCode::NOT_FOUND {
                break;
            }
            let response = response
                .error_for_status()
                .with_context(|| format!("failed to fetch {page_url}"))?;
            let html = response.text().await?;
            if page == 1 {
                description = parse_description(&html)?;
            }
            let parsed = parse_films(&html)?;
            if parsed.is_empty() {
                if page == 1 {
                    bail!("no films found at {url}; check that the list is public");
                }
                break;
            }

            let before = films.len();
            for film in parsed {
                if seen.insert(film.lid.clone()) {
                    films.push(film);
                }
            }
            if films.len() == before || !has_next_page(&html) {
                break;
            }
            page += 1;
        }
        Ok(SourceList { description, films })
    }

    pub async fn fetch_owned_list(&self, url: &Url) -> Result<SourceList> {
        let mut films = Vec::new();
        let mut seen = HashSet::new();
        let mut description = String::new();
        let mut page = 1_u32;

        loop {
            let page_url = if page == 1 {
                url.clone()
            } else {
                url.join(&format!("page/{page}/"))?
            };
            let (status, html) = self.read_request(page_url.as_str())?;
            if page > 1 && status == StatusCode::NOT_FOUND {
                break;
            }
            if !status.is_success() {
                bail!("failed to fetch {page_url}: HTTP {status}");
            }
            if page == 1 {
                description = parse_description(&html)?;
            }
            let parsed = parse_films(&html)?;
            if parsed.is_empty() {
                if page == 1 {
                    bail!("no films found at {url}");
                }
                break;
            }

            let before = films.len();
            for film in parsed {
                if seen.insert(film.lid.clone()) {
                    films.push(film);
                }
            }
            if films.len() == before || !has_next_page(&html) {
                break;
            }
            page += 1;
        }
        Ok(SourceList { description, films })
    }

    pub async fn fetch_recent_tagged_diary(
        &self,
        username: &str,
        tag_slug: &str,
    ) -> Result<Vec<Film>> {
        let url = Url::parse(&format!("{WEBSITE}/{username}/tag/{tag_slug}/diary/"))?;
        let (status, html) = self.read_request(url.as_str())?;
        if !status.is_success() {
            bail!("failed to fetch {url}: HTTP {status}");
        }
        let films = parse_diary_films(&html)?;
        if films.is_empty() {
            bail!("no tagged diary entries found at {url}");
        }
        Ok(films)
    }

    pub async fn inspect_target(&self, target_url: &Url) -> Result<Option<TargetList>> {
        let edit_url = target_url.join("edit/")?;
        let (status, html) = self.authenticated_request("GET", edit_url.as_str(), None)?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("failed to inspect {edit_url}: HTTP {status}");
        }
        parse_target_list(&html).map(Some)
    }

    pub async fn find_list_url(&self, username: &str, name: &str) -> Result<Option<Url>> {
        let base = Url::parse(&format!("{WEBSITE}/{username}/lists/"))?;
        let selector = Selector::parse("article.list-summary h2.name a[href]")
            .map_err(|error| anyhow!("invalid selector: {error}"))?;
        let mut page = 1_u32;

        loop {
            let page_url = if page == 1 {
                base.clone()
            } else {
                base.join(&format!("page/{page}/"))?
            };
            let (status, html) = self.authenticated_request("GET", page_url.as_str(), None)?;
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                bail!("failed to fetch {page_url}: HTTP {status}");
            }
            let document = Html::parse_document(&html);
            for link in document.select(&selector) {
                let title = link.text().collect::<String>();
                if title.trim().eq_ignore_ascii_case(name) {
                    let href = link.value().attr("href").context("list link has no URL")?;
                    return Ok(Some(base.join(href)?));
                }
            }
            if !has_next_page(&html) {
                return Ok(None);
            }
            page += 1;
        }
    }

    pub async fn create_list(
        &self,
        name: &str,
        description: &str,
        films: &[Film],
    ) -> Result<TargetList> {
        let payload = CreateList {
            published: true,
            name,
            share_policy: "You",
            ranked: false,
            description,
            tags: vec![],
            entries: films
                .iter()
                .map(|film| ListUpdateEntry::Add {
                    listable: film.lid.clone(),
                })
                .collect(),
        };
        let saved: SavedList = self.send_save("POST", "/lists", &payload).await?;
        Ok(TargetList {
            lid: saved.id,
            version: saved.version.unwrap_or(1),
        })
    }

    pub async fn replace_list(
        &self,
        target: &TargetList,
        name: &str,
        description: &str,
        films: &[Film],
    ) -> Result<TargetList> {
        let mut entries = Vec::with_capacity(films.len() + 1);
        entries.push(ListUpdateEntry::Clear);
        entries.extend(films.iter().map(|film| ListUpdateEntry::Add {
            listable: film.lid.clone(),
        }));
        let payload = UpdateList {
            version: target.version,
            published: true,
            name,
            share_policy: "You",
            ranked: false,
            description,
            tags: vec![],
            entries,
        };
        let saved: SavedList = self
            .send_save("PATCH", &format!("/list/{}", target.lid), &payload)
            .await?;
        Ok(TargetList {
            lid: saved.id,
            version: saved.version.unwrap_or(target.version + 1),
        })
    }

    async fn send_save<T: Serialize + ?Sized>(
        &self,
        method: &str,
        path: &str,
        payload: &T,
    ) -> Result<SavedList> {
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let body = serde_json::to_string(payload)?;
        let (status, body) =
            self.authenticated_request(method.as_str(), &format!("{API_BASE}{path}"), Some(&body))?;
        if status == StatusCode::UNAUTHORIZED {
            bail!("Letterboxd authentication expired or cannot authorize API writes (HTTP 401)");
        }
        if !status.is_success() {
            bail!(
                "Letterboxd save failed with HTTP {status}: {}",
                compact(&body)
            );
        }
        let parsed: SaveResponse = serde_json::from_str(&body)
            .with_context(|| format!("invalid Letterboxd save response: {}", compact(&body)))?;
        let errors: Vec<_> = parsed
            .messages
            .iter()
            .filter(|message| message.kind == "Error")
            .collect();
        if !errors.is_empty() {
            let text = errors
                .iter()
                .map(|error| format!("{}: {}", error.code, error.message))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("Letterboxd rejected the list: {text}");
        }
        let saved = parsed
            .data
            .context("Letterboxd returned success without list data")?;
        eprintln!("Saved '{}' (LID {})", saved.name, saved.id);
        Ok(saved)
    }

    fn authenticated_request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<(StatusCode, String)> {
        let user_cookie = self
            .user_cookie
            .as_deref()
            .context("Letterboxd session cookies are not configured")?;
        let csrf = self
            .csrf
            .as_deref()
            .context("Letterboxd CSRF cookie is not configured")?;
        let binary = env::var("CURL_IMPERSONATE").unwrap_or_else(|_| "curl_chrome136".into());
        let mut command = Command::new(binary);
        command
            .args([
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "30",
                "--variable",
                "%LETTERBOXD_USER_COOKIE",
                "--variable",
                "%LETTERBOXD_CSRF_COOKIE",
                "--expand-header",
                "Cookie: letterboxd.user={{LETTERBOXD_USER_COOKIE}}; com.xk72.webparts.csrf={{LETTERBOXD_CSRF_COOKIE}}",
                "--expand-header",
                "x-csrf-token: {{LETTERBOXD_CSRF_COOKIE}}",
                "--header",
                "Origin: https://letterboxd.com",
                "--header",
                "Referer: https://letterboxd.com/",
                "--header",
                "Accept: application/json",
                "--write-out",
                "\n__HTTP_STATUS__:%{http_code}",
                "--request",
                method,
                url,
            ])
            .env("LETTERBOXD_USER_COOKIE", user_cookie)
            .env("LETTERBOXD_CSRF_COOKIE", csrf)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if body.is_some() {
            command
                .args(["--header", "Content-Type: application/json; charset=UTF-8"])
                .args(["--data-binary", "@-"])
                .stdin(Stdio::piped());
        }

        let mut child = command
            .spawn()
            .context("curl_chrome136 is required for authenticated Letterboxd requests")?;
        if let Some(body) = body {
            child
                .stdin
                .take()
                .context("failed to open curl stdin")?
                .write_all(body.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "authenticated request failed: {}",
                compact(&String::from_utf8_lossy(&output.stderr))
            );
        }
        let output = String::from_utf8(output.stdout).context("curl returned invalid text")?;
        let (body, status) = output
            .rsplit_once("\n__HTTP_STATUS__:")
            .context("curl response did not contain an HTTP status")?;
        let status = StatusCode::from_u16(status.trim().parse()?)?;
        Ok((status, body.to_owned()))
    }

    fn browser_request(&self, url: &str) -> Result<(StatusCode, String)> {
        let binary = env::var("CURL_IMPERSONATE").unwrap_or_else(|_| "curl_chrome136".into());
        let output = Command::new(binary)
            .args([
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "30",
                "--header",
                "Accept: text/html",
                "--write-out",
                "\n__HTTP_STATUS__:%{http_code}",
                url,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("curl_chrome136 is required for browser-compatible Letterboxd reads")?;
        if !output.status.success() {
            bail!(
                "browser-compatible request failed: {}",
                compact(&String::from_utf8_lossy(&output.stderr))
            );
        }
        let output = String::from_utf8(output.stdout).context("curl returned invalid text")?;
        let (body, status) = output
            .rsplit_once("\n__HTTP_STATUS__:")
            .context("curl response did not contain an HTTP status")?;
        let status = StatusCode::from_u16(status.trim().parse()?)?;
        Ok((status, body.to_owned()))
    }

    fn read_request(&self, url: &str) -> Result<(StatusCode, String)> {
        if self.user_cookie.is_some() {
            self.authenticated_request("GET", url, None)
        } else {
            self.browser_request(url)
        }
    }
}

fn compact(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

fn parse_films(html: &str) -> Result<Vec<Film>> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("[data-postered-identifier][data-item-slug]")
        .map_err(|error| anyhow!("invalid selector: {error}"))?;
    let mut films = Vec::new();
    let mut seen = HashSet::new();

    for element in document.select(&selector) {
        let Some(identifier) = element.value().attr("data-postered-identifier") else {
            continue;
        };
        let parsed: Value = serde_json::from_str(identifier)
            .with_context(|| format!("invalid data-postered-identifier: {identifier}"))?;
        if parsed.get("type").and_then(Value::as_str) != Some("film") {
            continue;
        }
        let Some(lid) = parsed.get("lid").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(lid.to_owned()) {
            continue;
        }
        films.push(Film {
            lid: lid.to_owned(),
            slug: element
                .value()
                .attr("data-item-slug")
                .unwrap_or_default()
                .to_owned(),
            name: element
                .value()
                .attr("data-item-name")
                .unwrap_or_default()
                .to_owned(),
        });
    }
    Ok(films)
}

fn parse_diary_films(html: &str) -> Result<Vec<Film>> {
    let document = Html::parse_document(html);
    let row_selector = Selector::parse("tr.diary-entry-row")
        .map_err(|error| anyhow!("invalid selector: {error}"))?;
    let film_selector = Selector::parse("[data-postered-identifier][data-item-slug]")
        .map_err(|error| anyhow!("invalid selector: {error}"))?;
    let mut films = Vec::new();
    let mut seen = HashSet::new();

    for row in document.select(&row_selector) {
        let Some(element) = row.select(&film_selector).next() else {
            continue;
        };
        let Some(film) = parse_film(element)? else {
            continue;
        };
        if seen.insert(film.lid.clone()) {
            films.push(film);
        }
    }
    Ok(films)
}

fn parse_film(element: scraper::ElementRef<'_>) -> Result<Option<Film>> {
    let Some(identifier) = element.value().attr("data-postered-identifier") else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(identifier)
        .with_context(|| format!("invalid data-postered-identifier: {identifier}"))?;
    if parsed.get("type").and_then(Value::as_str) != Some("film") {
        return Ok(None);
    }
    let Some(lid) = parsed.get("lid").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(Some(Film {
        lid: lid.to_owned(),
        slug: element
            .value()
            .attr("data-item-slug")
            .unwrap_or_default()
            .to_owned(),
        name: element
            .value()
            .attr("data-item-name")
            .unwrap_or_default()
            .to_owned(),
    }))
}

pub fn merge_recent_tagged_films(target: &[Film], recent: &[Film]) -> Result<Option<Vec<Film>>> {
    if target.is_empty() || recent.is_empty() {
        bail!("target list and recent tagged diary must not be empty");
    }

    let target_positions = target
        .iter()
        .enumerate()
        .map(|(index, film)| (film.lid.as_str(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let chronological = recent.iter().rev().collect::<Vec<_>>();
    let mut runs = Vec::new();
    let mut current: Option<(usize, usize, usize)> = None;

    for (index, film) in chronological.iter().enumerate() {
        let Some(position) = target_positions.get(film.lid.as_str()).copied() else {
            continue;
        };
        match current {
            Some((start, last_position, len))
                if position > last_position && position - last_position <= 3 =>
            {
                current = Some((start, position, len + 1));
            }
            Some((start, last_position, len)) => {
                runs.push((start, index - 1, last_position, len));
                current = Some((index, position, 1));
            }
            None => current = Some((index, position, 1)),
        }
    }
    if let Some((start, last_position, len)) = current {
        runs.push((start, chronological.len() - 1, last_position, len));
    }

    let (start, _, boundary, overlap) = runs
        .into_iter()
        .max_by_key(|(_, _, _, len)| *len)
        .context("recent tagged diary does not overlap the target list")?;
    if overlap < 2 {
        bail!(
            "could not identify the watched-list boundary from recent tagged entries; refusing to reorder the list"
        );
    }

    let watched = target[..=boundary]
        .iter()
        .map(|film| film.lid.as_str())
        .collect::<HashSet<_>>();
    let anchor_lid = &chronological[start].lid;
    let mut merged = target.to_vec();
    let mut cursor = merged
        .iter()
        .position(|film| &film.lid == anchor_lid)
        .context("watched-list anchor disappeared while merging")?;

    for film in &chronological[start + 1..] {
        match merged.iter().position(|current| current.lid == film.lid) {
            Some(position) if watched.contains(film.lid.as_str()) && position > cursor => {
                cursor = position;
            }
            Some(position) => {
                let existing = merged.remove(position);
                if position < cursor {
                    cursor -= 1;
                }
                cursor += 1;
                merged.insert(cursor, existing);
            }
            None => {
                cursor += 1;
                merged.insert(cursor, (*film).clone());
            }
        }
    }

    if merged == target {
        Ok(None)
    } else {
        Ok(Some(merged))
    }
}

fn parse_description(html: &str) -> Result<String> {
    let document = Html::parse_document(html);
    let description_selector = Selector::parse(".list-title-intro .body-text.-prose.-hero")
        .map_err(|error| anyhow!("invalid selector: {error}"))?;
    let paragraph_selector =
        Selector::parse("p").map_err(|error| anyhow!("invalid selector: {error}"))?;
    let Some(description) = document.select(&description_selector).next() else {
        return Ok(String::new());
    };

    let paragraphs = description
        .select(&paragraph_selector)
        .map(|paragraph| paragraph.text().collect::<String>().trim().to_owned())
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>();
    if paragraphs.is_empty() {
        Ok(description.text().collect::<String>().trim().to_owned())
    } else {
        Ok(paragraphs.join("\n\n"))
    }
}

fn has_next_page(html: &str) -> bool {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a.next").expect("static selector is valid");
    document.select(&selector).next().is_some()
}

fn parse_target_list(html: &str) -> Result<TargetList> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("[data-component-class=ListEditor]")
        .map_err(|error| anyhow!("invalid selector: {error}"))?;
    let element = document
        .select(&selector)
        .next()
        .context("target list editor was not found; check ownership and session cookies")?;
    let lid = element
        .value()
        .attr("data-list-lid")
        .context("target list has no Letterboxd ID")?
        .to_owned();
    let version = element
        .value()
        .attr("data-list-version")
        .context("target list has no version")?
        .parse()
        .context("target list version is not a number")?;
    Ok(TargetList { lid, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn film(lid: &str) -> Film {
        Film {
            lid: lid.to_owned(),
            slug: lid.to_lowercase(),
            name: lid.to_owned(),
        }
    }

    #[test]
    fn parses_films_in_order_and_deduplicates() {
        let html = r#"
<div data-postered-identifier='{&quot;lid&quot;:&quot;2b34&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="dracula" data-item-name="Dracula (1931)"></div>
<div data-postered-identifier='{&quot;lid&quot;:&quot;1Azs&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="draculas-daughter" data-item-name="Dracula's Daughter (1936)"></div>
<div data-postered-identifier='{&quot;lid&quot;:&quot;2b34&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="dracula" data-item-name="Dracula (1931)"></div>
"#;
        let films = parse_films(html).unwrap();
        assert_eq!(films.len(), 2);
        assert_eq!(films[0].lid, "2b34");
        assert_eq!(films[1].slug, "draculas-daughter");
    }

    #[test]
    fn parses_target_metadata() {
        let html = r#"<div data-component-class="ListEditor" data-list-lid="abc123" data-list-version="7"></div>"#;
        let list = parse_target_list(html).unwrap();
        assert_eq!(list.lid, "abc123");
        assert_eq!(list.version, 7);
    }

    #[test]
    fn parses_authored_description_in_paragraphs() {
        let html = r#"
<div class="list-title-intro">
  <div class="body-text -prose -hero clear js-collapsible-text">
    <p>First paragraph with <a href="https://example.com">a link</a>.</p>
    <p>Second paragraph.</p>
  </div>
</div>
"#;
        assert_eq!(
            parse_description(html).unwrap(),
            "First paragraph with a link.\n\nSecond paragraph."
        );
    }

    #[test]
    fn missing_authored_description_is_empty() {
        assert_eq!(parse_description("<html></html>").unwrap(), "");
    }

    #[test]
    fn parses_recent_diary_films_and_deduplicates_rewatches() {
        let html = r#"
<div data-postered-identifier='{&quot;lid&quot;:&quot;ignore&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="ignore" data-item-name="Ignore"></div>
<table><tbody>
<tr class="diary-entry-row"><td><div data-postered-identifier='{&quot;lid&quot;:&quot;new&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="new-film" data-item-name="New Film"></div></td></tr>
<tr class="diary-entry-row"><td><div data-postered-identifier='{&quot;lid&quot;:&quot;old&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="old-film" data-item-name="Old Film"></div></td></tr>
<tr class="diary-entry-row"><td><div data-postered-identifier='{&quot;lid&quot;:&quot;new&quot;,&quot;type&quot;:&quot;film&quot;}' data-item-slug="new-film" data-item-name="New Film"></div></td></tr>
</tbody></table>
"#;
        let films = parse_diary_films(html).unwrap();
        assert_eq!(
            films
                .iter()
                .map(|film| film.lid.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "old"]
        );
    }

    #[test]
    fn inserts_recent_tagged_films_at_watched_boundary_and_moves_tail_entries() {
        let target = vec![
            film("old"),
            film("anchor"),
            film("unwatched-1"),
            film("unwatched-2"),
            film("unwatched-3"),
            film("unwatched-4"),
            film("planned-now-watched"),
            film("unwatched-5"),
        ];
        let recent = vec![
            film("newest"),
            film("planned-now-watched"),
            film("new"),
            film("anchor"),
            film("old"),
        ];

        let merged = merge_recent_tagged_films(&target, &recent)
            .unwrap()
            .unwrap();
        assert_eq!(
            merged,
            vec![
                film("old"),
                film("anchor"),
                film("new"),
                film("planned-now-watched"),
                film("newest"),
                film("unwatched-1"),
                film("unwatched-2"),
                film("unwatched-3"),
                film("unwatched-4"),
                film("unwatched-5"),
            ]
        );
        assert!(
            merge_recent_tagged_films(&merged, &recent)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn refuses_to_reorder_without_a_stable_boundary() {
        let error = merge_recent_tagged_films(
            &[film("old"), film("unwatched")],
            &[film("new"), film("old")],
        )
        .unwrap_err();
        assert!(error.to_string().contains("refusing to reorder"));
    }
}
