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
}
