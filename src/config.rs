use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub username: String,
    #[serde(default)]
    pub lists: Vec<ListPair>,
    #[serde(default)]
    pub tagged_lists: Vec<TaggedList>,
}

#[derive(Debug, Deserialize)]
pub struct ListPair {
    pub source: Url,
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct TaggedList {
    pub tag: String,
    pub target: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.username.is_empty()
            || !self.username.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            })
        {
            bail!("username must contain only letters, numbers, hyphens, and underscores");
        }
        if self.lists.is_empty() && self.tagged_lists.is_empty() {
            bail!("configure at least one mirrored or tagged list");
        }
        for (index, pair) in self.lists.iter().enumerate() {
            validate_list_url(&pair.source, &format!("lists[{index}].source"))?;
            if slugify(&pair.target).is_empty() {
                bail!("lists[{index}].target must contain a list name");
            }
        }
        for (index, tagged) in self.tagged_lists.iter().enumerate() {
            if slugify(&tagged.tag).is_empty() {
                bail!("tagged_lists[{index}].tag must contain a tag");
            }
            if slugify(&tagged.target).is_empty() {
                bail!("tagged_lists[{index}].target must contain a list name");
            }
        }
        Ok(())
    }
}

impl ListPair {
    pub fn source_username(&self) -> &str {
        self.source
            .path_segments()
            .and_then(|mut segments| segments.next())
            .expect("validated source URL has a username")
    }

    pub fn target_url(&self, username: &str) -> Url {
        target_url(username, &self.target)
    }
}

impl TaggedList {
    pub fn tag_slug(&self) -> String {
        slugify(&self.tag)
    }

    pub fn target_url(&self, username: &str) -> Url {
        target_url(username, &self.target)
    }
}

fn target_url(username: &str, name: &str) -> Url {
    Url::parse(&format!(
        "https://letterboxd.com/{username}/list/{}/",
        slugify(name)
    ))
    .expect("validated username and target name form a URL")
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else if character.is_ascii_whitespace() || character == '-' {
            separator = true;
        }
    }
    slug
}

fn validate_list_url(url: &Url, field: &str) -> Result<()> {
    if url.scheme() != "https" || url.host_str() != Some("letterboxd.com") {
        bail!("{field} must be an https://letterboxd.com URL");
    }
    let parts: Vec<_> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 3 || parts[1] != "list" {
        bail!("{field} must have the form https://letterboxd.com/user/list/slug/");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_url_pairs() {
        let config: Config = serde_yaml::from_str(
            r#"
username: shadrunov
lists:
  - source: https://letterboxd.com/alice/list/one/
    target: First List
  - source: https://letterboxd.com/bob/list/two/
    target: "a&a: vampire season"
tagged_lists:
  - tag: w/ andrei
    target: Andrew
"#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.lists.len(), 2);
        assert_eq!(config.tagged_lists.len(), 1);
        assert_eq!(config.lists[1].source_username(), "bob");
        assert_eq!(
            config.lists[1].target_url(&config.username).as_str(),
            "https://letterboxd.com/shadrunov/list/aa-vampire-season/"
        );
        assert_eq!(config.tagged_lists[0].tag_slug(), "w-andrei");
        assert_eq!(
            config.tagged_lists[0].target_url(&config.username).as_str(),
            "https://letterboxd.com/shadrunov/list/andrew/"
        );
    }
}
