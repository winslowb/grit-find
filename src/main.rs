use anyhow::{Context, Result, anyhow};
use clap::Parser;
use dialoguer::{Input, Select, theme::ColorfulTheme};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};
use serde::Deserialize;
use std::{env, fs::File, io::Write, path::PathBuf, time::Duration};
use tokio::time::sleep;

const DISPLAY_PAGE_SIZE: usize = 25; // show 25 per page
const MAX_RESULTS: usize = 100; // fetch up to 100 total results

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

    /// 1-based display page to start from (each page shows up to 25 results)
    #[arg(long, default_value_t = 1)]
    page: usize,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<Repo>,
}

#[derive(Debug, Deserialize, Clone)]
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
    // Fetch up to MAX_RESULTS once; paging is local (no extra API calls)
    let repos = fetch_all_repos(&github, &query).await?;
    if repos.is_empty() {
        println!("No repositories found for query: {query}");
        return Ok(());
    }

    let total_pages = (repos.len() + DISPLAY_PAGE_SIZE - 1) / DISPLAY_PAGE_SIZE;
    let mut page = args.page.max(1).min(total_pages.max(1));

    let repo = loop {
        let start = (page - 1) * DISPLAY_PAGE_SIZE;
        let end = (start + DISPLAY_PAGE_SIZE).min(repos.len());
        let slice = &repos[start..end];
        println!(
            "\nShowing page {}/{} ({} results this page, total {}). Enter number to select, 'n' for next page, 'p' for previous page, or 'c' to cancel.",
            page,
            total_pages,
            slice.len(),
            repos.len()
        );

        for (idx, r) in slice.iter().enumerate() {
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
            if page < total_pages {
                page += 1;
            } else {
                println!("No more pages.");
            }
            continue;
        } else if choice.eq_ignore_ascii_case("p") {
            if page > 1 {
                page -= 1;
            } else {
                println!("Already at the first page.");
            }
            continue;
        } else if choice.eq_ignore_ascii_case("c") {
            return Ok(());
        }

        if let Ok(num) = choice.parse::<usize>() {
            if num >= 1 && num <= slice.len() {
                break slice[num - 1].clone();
            }
        }

        println!(
            "Invalid choice. Please enter a number between 1-{}, n, p, or c.",
            slice.len()
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

    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", token))?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .user_agent("grit-find")
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build GitHub HTTP client")
}

async fn fetch_repos_page(
    client: &reqwest::Client,
    query: &str,
    per_page: usize,
    page: usize,
) -> Result<Vec<Repo>> {
    let q = format!("{query} is:public");
    let url = Url::parse_with_params(
        "https://api.github.com/search/repositories",
        [
            ("q", q.as_str()),
            ("per_page", &per_page.to_string()),
            ("page", &page.to_string()),
            ("sort", "stars"),
            ("order", "desc"),
        ],
    )?;

    let mut attempts = 0;
    loop {
        attempts += 1;
        let res = client.get(url.clone()).send().await?;
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

async fn fetch_all_repos(client: &reqwest::Client, query: &str) -> Result<Vec<Repo>> {
    let mut all = Vec::new();
    let mut page = 1;
    let per_page = 100; // minimize API calls

    while all.len() < MAX_RESULTS {
        let fetched = fetch_repos_page(client, query, per_page, page).await?;
        if fetched.is_empty() {
            break;
        }
        let fetched_len = fetched.len();
        all.extend(fetched);
        if all.len() >= MAX_RESULTS {
            all.truncate(MAX_RESULTS);
            break;
        }
        if fetched_len < per_page {
            break; // no more pages
        }
        page += 1;
    }
    Ok(all)
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
            ResponseFormat,
        },
    };
    let client = Client::new();
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
        // Ask the API to emit strict JSON; still defensively parse below.
        .response_format(ResponseFormat::JsonObject)
        .build()?;

    let resp = client.chat().create(req).await?;
    let content = resp
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .ok_or_else(|| anyhow!("OpenAI returned empty content"))?;

    parse_query_from_content(content)
}

#[derive(Deserialize)]
struct Suggestion {
    query: String,
}

fn parse_query_from_content(content: &str) -> Result<String> {
    // Try as-is first.
    if let Ok(s) = serde_json::from_str::<Suggestion>(content) {
        return Ok(s.query);
    }

    // Handle common model behaviour of wrapping JSON in ```json fences.
    let trimmed = content.trim();
    if let Some(stripped) = strip_code_fence(trimmed) {
        if let Ok(s) = serde_json::from_str::<Suggestion>(&stripped) {
            return Ok(s.query);
        }
    }

    Err(anyhow!(
        "OpenAI response was not valid JSON (first 200 chars): {}",
        truncate_preview(content, 200)
    ))
}

fn strip_code_fence(input: &str) -> Option<String> {
    if !input.starts_with("```") {
        return None;
    }

    let mut lines = input.lines();
    // Drop opening fence (maybe with language tag).
    lines.next()?;

    let mut body_lines = Vec::new();
    for line in lines {
        if line.trim() == "```" {
            break;
        }
        body_lines.push(line);
    }

    if body_lines.is_empty() {
        return None;
    }

    Some(body_lines.join("\n").trim().to_string())
}

fn truncate_preview(content: &str, max_chars: usize) -> String {
    let mut preview = content.chars().take(max_chars).collect::<String>();
    if content.chars().count() > max_chars {
        preview.push_str("…");
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::parse_query_from_content;

    #[test]
    fn parses_plain_json() {
        let content = r#"{"query":"foo bar"}"#;
        let q = parse_query_from_content(content).expect("should parse plain json");
        assert_eq!(q, "foo bar");
    }

    #[test]
    fn parses_code_fenced_json() {
        let content = "```json\n{\"query\":\"ripgrep\"}\n```";
        let q = parse_query_from_content(content).expect("should parse fenced json");
        assert_eq!(q, "ripgrep");
    }
}
