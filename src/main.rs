use anyhow::{Context, Result, anyhow};
use clap::Parser;
use dialoguer::{Input, Select, theme::ColorfulTheme};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};
use tokio::time::sleep;

const FETCH_PER_PAGE: usize = 100;

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

    /// 1-based GitHub results page to start from (each page fetches up to 100 results)
    #[arg(long, default_value_t = 1)]
    page: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct SearchResponse {
    items: Vec<Repo>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    queries: HashMap<String, QueryCache>,
}

#[derive(Serialize, Deserialize)]
struct QueryCache {
    pages: HashMap<usize, Vec<Repo>>,
    fully_fetched: bool,
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

    let mut gh_page = args.page.max(1);
    let page_size = FETCH_PER_PAGE; // display 100 at a time

    let repo = loop {
        let repos = search_repos(&github, &query, page_size, gh_page).await?;
        if repos.is_empty() {
            if gh_page == args.page {
                println!("No repositories found for query: {query}");
            } else {
                println!("No more results.");
            }
            return Ok(());
        }

        println!(
            "\nShowing GitHub page {} (up to {} results). Enter number to select, 'n' for next page, 'p' for previous page, or 'c' to cancel.",
            gh_page, page_size
        );

        for (idx, r) in repos.iter().enumerate() {
            println!(
                "{:>3}. \u{001b}[1m{}\u{001b}[0m (★{}): {}",
                idx + 1,
                r.full_name,
                r.stargazers_count,
                r.description
                    .clone()
                    .unwrap_or_else(|| "no description".into())
            );
        }

        let choice: String = Input::with_theme(&theme)
            .with_prompt("Choice (number, n/p, c)")
            .interact_text()?;
        let choice = choice.trim();

        if choice.eq_ignore_ascii_case("n") {
            gh_page += 1;
            continue;
        } else if choice.eq_ignore_ascii_case("p") {
            if gh_page > 1 {
                gh_page -= 1;
            } else {
                println!("Already at the first page.");
            }
            continue;
        } else if choice.eq_ignore_ascii_case("c") {
            return Ok(());
        }

        if let Ok(num) = choice.parse::<usize>() {
            if num >= 1 && num <= repos.len() {
                break repos[num - 1].clone();
            }
        }

        println!(
            "Invalid choice. Please enter a number between 1-{}, n, p, or c.",
            repos.len()
        );
    };

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

async fn search_repos(
    client: &reqwest::Client,
    query: &str,
    per_page: usize,
    page: usize,
) -> Result<Vec<Repo>> {
    let per_page = per_page.min(100).max(1);
    let page = page.max(1);

    // Try cache first
    let mut cache = load_cache().unwrap_or_default();
    if let Some(entry) = cache.queries.get(query) {
        if let Some(cached_page) = entry.pages.get(&page) {
            return Ok(cached_page.clone());
        }
        if entry.fully_fetched && page > entry.pages.len() {
            return Ok(Vec::new());
        }
    }

    let fetched = fetch_repos_page(client, query, per_page, page).await?;

    let fully_fetched = fetched.len() < per_page;
    let entry = cache
        .queries
        .entry(query.to_string())
        .or_insert(QueryCache {
            pages: HashMap::new(),
            fully_fetched: false,
        });
    entry.pages.insert(page, fetched.clone());
    if fully_fetched {
        entry.fully_fetched = true;
    }
    save_cache(&cache)?;

    Ok(fetched)
}

async fn fetch_repos_page(
    client: &reqwest::Client,
    query: &str,
    per_page: usize,
    page: usize,
) -> Result<Vec<Repo>> {
    let url = format!(
        "https://api.github.com/search/repositories?q={query}+is:public&per_page={per_page}&page={page}&sort=stars&order=desc"
    );

    let mut attempts = 0;
    loop {
        attempts += 1;
        let res = client.get(&url).send().await?;
        if res.status() == StatusCode::TOO_MANY_REQUESTS {
            let wait = res
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5);
            println!("Hit GitHub rate limit. Waiting {wait} seconds...");
            sleep(Duration::from_secs(wait)).await;
            if attempts < 3 {
                continue;
            }
        }

        let res = res.error_for_status().context("GitHub search failed")?;

        let search: SearchResponse = res.json().await?;

        // Filter by having a release available
        let mut with_releases = Vec::new();
        for repo in search.items {
            if let Ok(_) = latest_release(client, &repo.full_name).await {
                with_releases.push(repo);
            }
        }

        return Ok(with_releases);
    }
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
        Client,
        types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent, CreateChatCompletionRequestArgs,
        },
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

fn cache_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.cache_dir().join("grit-find").join("cache.json"))
}

fn load_cache() -> Result<Cache> {
    if let Some(path) = cache_path() {
        if let Ok(mut f) = File::open(&path) {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            let cache: Cache = serde_json::from_str(&buf)?;
            return Ok(cache);
        }
    }
    Ok(Cache::default())
}

fn save_cache(cache: &Cache) -> Result<()> {
    if let Some(path) = cache_path() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = File::create(path)?;
        let data = serde_json::to_string(cache)?;
        f.write_all(data.as_bytes())?;
    }
    Ok(())
}
