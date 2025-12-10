use anyhow::{anyhow, Context, Result};
use clap::Parser;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::{env, fs::File, io::Write, path::PathBuf, time::Duration};

#[derive(Parser, Debug)]
#[command(version, about = "Search and download GitHub releases")]
struct Cli {
    /// Search terms (fallback to interactive prompt)
    query: Vec<String>,

    /// Use OpenAI to help craft the search query from a short description
    #[arg(long)]
    ai: bool,

    /// Destination directory for downloaded asset
    #[arg(short, long, value_name = "DIR", default_value = ".")]
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<Repo>,
}

#[derive(Debug, Deserialize)]
struct Repo {
    full_name: String,
    description: Option<String>,
    stargazers_count: u64,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    name: Option<String>,
    assets: Vec<Asset>,
    html_url: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    let theme = ColorfulTheme::default();

    let query = if args.ai {
        let description: String = if args.query.is_empty() {
            Input::with_theme(&theme)
                .with_prompt("Describe what you need (OpenAI will craft the search query)")
                .interact_text()?
        } else {
            args.query.join(" ")
        };
        println!("Using OpenAI to propose a GitHub search query...");
        ai_suggest_query(&description).await?
    } else if args.query.is_empty() {
        Input::with_theme(&theme)
            .with_prompt("GitHub search keywords")
            .interact_text()?
    } else {
        args.query.join(" ")
    };

    let github = github_client()?;

    let repos = search_repos(&github, &query).await?;
    if repos.is_empty() {
        println!("No repositories found for query: {query}");
        return Ok(());
    }

    let choices: Vec<String> = repos
        .iter()
        .map(|r| {
            format!(
                "{} (★{}): {}",
                r.full_name,
                r.stargazers_count,
                r.description
                    .clone()
                    .unwrap_or_else(|| "no description".into())
            )
        })
        .collect();

    let selection = Select::with_theme(&theme)
        .with_prompt("Select a repository with releases")
        .items(&choices)
        .default(0)
        .interact()?;

    let repo = &repos[selection];
    let release = latest_release(&github, &repo.full_name).await?;
    if release.assets.is_empty() {
        println!(
            "Latest release '{}' has no downloadable assets.",
            release.tag_name
        );
        return Ok(());
    }

    let asset_choices: Vec<String> = release
        .assets
        .iter()
        .map(|a| format!("{} ({:.2} MB)", a.name, a.size as f64 / 1_048_576.0))
        .collect();

    let asset_idx = Select::with_theme(&theme)
        .with_prompt(format!(
            "Select asset to download from {} ({})",
            release.tag_name,
            release
                .name
                .clone()
                .unwrap_or_else(|| "unnamed release".into())
        ))
        .items(&asset_choices)
        .default(0)
        .interact()?;

    let asset = release.assets[asset_idx].clone();
    let dest_dir = &args.output;
    tokio::fs::create_dir_all(dest_dir).await?;
    let dest = dest_dir.join(&asset.name);

    println!("Downloading {} to {}", asset.name, dest.to_string_lossy());
    download_asset(&github, &asset, &dest).await?;
    println!("Done.");

    Ok(())
}

fn github_client() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_str("grit-find (github.com)")?);
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );

    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent("grit-find")
        .timeout(Duration::from_secs(20));

    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", token))?;
        value.set_sensitive(true);
        builder = builder.default_headers({
            let mut h = HeaderMap::new();
            h.insert(AUTHORIZATION, value);
            h
        });
    }

    builder
        .build()
        .context("failed to build GitHub HTTP client")
}

async fn search_repos(client: &reqwest::Client, query: &str) -> Result<Vec<Repo>> {
    let url = format!(
        "https://api.github.com/search/repositories?q={query}+is:public&per_page=25&sort=stars&order=desc"
    );
    let res = client
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .context("GitHub search failed")?;
    let search: SearchResponse = res.json().await?;

    // Filter by having a release available
    let mut with_releases = Vec::new();
    for repo in search.items {
        if let Ok(_) = latest_release(client, &repo.full_name).await {
            with_releases.push(repo);
        }
    }

    Ok(with_releases)
}

async fn latest_release(client: &reqwest::Client, full_name: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{full_name}/releases/latest");
    let res = client
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("Failed to fetch latest release for {full_name}"))?;
    Ok(res.json::<Release>().await?)
}

async fn download_asset(client: &reqwest::Client, asset: &Asset, dest: &PathBuf) -> Result<()> {
    let resp = client
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()
        .context("asset download request failed")?;

    let total_size = asset.size;
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
        )?
        .progress_chars("#>-"),
    );

    let mut file = File::create(dest).context("create destination file")?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        pb.inc(chunk.len() as u64);
    }
    pb.finish_with_message("downloaded");
    Ok(())
}

async fn ai_suggest_query(description: &str) -> Result<String> {
    use async_openai::{
        types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent, CreateChatCompletionRequestArgs,
        },
        Client,
    };
    let client = Client::new();
    #[derive(Deserialize)]
    struct Suggestion {
        query: String,
    }

    let prompt = format!(
        "You are helping craft a concise GitHub search query to find repositories with releases. \
Description: \"{}\". Respond as JSON: {{\"query\": \"...\"}} with no extra text.",
        description
    );

    let user_msg = ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: ChatCompletionRequestUserMessageContent::Text(prompt),
        name: None,
    });

    let req = CreateChatCompletionRequestArgs::default()
        .model("gpt-4o-mini")
        .messages([user_msg])
        .build()?;

    let resp = client.chat().create(req).await?;
    let content = resp
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .ok_or_else(|| anyhow!("OpenAI returned empty content"))?;

    let suggestion: Suggestion = serde_json::from_str(content)?;
    Ok(suggestion.query)
}
