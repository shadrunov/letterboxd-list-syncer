mod config;
mod letterboxd;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use config::Config;
use letterboxd::{LetterboxdClient, merge_recent_tagged_films};
use url::Url;

const REPOSITORY_URL: &str = "https://github.com/shadrunov/letterboxd-list-syncer";

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,

    /// Read and compare without changing the target list.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    let client = LetterboxdClient::from_env()?;
    let list_urls = if args.dry_run {
        Default::default()
    } else {
        client.list_urls(&config.username).await?
    };

    for pair in &config.lists {
        let source = client.fetch_public_list(&pair.source).await?;
        let target_url = if args.dry_run {
            pair.target_url(&config.username)
        } else {
            list_urls
                .get(&pair.target.to_lowercase())
                .cloned()
                .unwrap_or_else(|| pair.target_url(&config.username))
        };
        let description =
            sync_description(&source.description, pair.source_username(), &pair.source);
        println!(
            "{} -> {} ({} films)",
            pair.source,
            target_url,
            source.films.len()
        );
        for (index, film) in source.films.iter().enumerate() {
            println!("{:>3}. {} [{}]", index + 1, film.name, film.slug);
        }

        if !args.dry_run {
            match client.inspect_target(&target_url).await? {
                Some(target) => {
                    client
                        .replace_list(&target, &pair.target, &description, &source.films)
                        .await?;
                    println!("Updated {target_url}");
                }
                None => {
                    let created = client
                        .create_list(&pair.target, &description, &source.films)
                        .await?;
                    println!("Created https://boxd.it/{}", created.lid);
                }
            }
        }
    }

    for tagged in &config.tagged_lists {
        let target_url = if args.dry_run {
            tagged.target_url(&config.username)
        } else {
            list_urls
                .get(&tagged.target.to_lowercase())
                .cloned()
                .unwrap_or_else(|| tagged.target_url(&config.username))
        };
        let target = if args.dry_run {
            None
        } else {
            Some(client.inspect_target(&target_url).await?.ok_or_else(|| {
                anyhow::anyhow!("tagged target list does not exist: {target_url}")
            })?)
        };
        let current = client.fetch_owned_list(&target_url).await?;
        let recent = client
            .fetch_recent_tagged_diary(&config.username, &tagged.tag_slug())
            .await?;

        match merge_recent_tagged_films(&current.films, &recent)? {
            Some(films) => {
                println!(
                    "Tag '{}' -> {} ({} -> {} films; recent entries reconciled)",
                    tagged.tag,
                    target_url,
                    current.films.len(),
                    films.len()
                );
                if !args.dry_run {
                    client
                        .replace_list(
                            target.as_ref().expect("normal run inspected the target"),
                            &tagged.target,
                            &current.description,
                            &films,
                        )
                        .await?;
                    println!("Updated {target_url}");
                }
            }
            None => println!("Tag '{}' -> {} is already current", tagged.tag, target_url),
        }
    }

    if args.dry_run {
        println!("Dry run complete; targets were not changed");
    }

    Ok(())
}

fn sync_description(source_description: &str, source_username: &str, source_url: &Url) -> String {
    let credit = format!(
        "synced from <a href=\"{source_url}\">{source_username}'s list</a> by <a href=\"{REPOSITORY_URL}\">letterboxd-list-syncer</a>"
    );
    if source_description.is_empty() {
        credit
    } else {
        format!("{source_description}\n\n{credit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_linked_sync_credit() {
        let source =
            Url::parse("https://letterboxd.com/cokewithglitter/list/aa-vampire-season/").unwrap();
        assert_eq!(
            sync_description("Original description", "cokewithglitter", &source),
            "Original description\n\nsynced from <a href=\"https://letterboxd.com/cokewithglitter/list/aa-vampire-season/\">cokewithglitter's list</a> by <a href=\"https://github.com/shadrunov/letterboxd-list-syncer\">letterboxd-list-syncer</a>"
        );
    }
}
