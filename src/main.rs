// ytmusic-dl — portable YouTube → MP3 downloader
// Firefox is opened via Selenium (thirtyfour + geckodriver).
// A download button is injected next to the video title.
// Pressing it closes the browser and runs yt-dlp with a progress bar.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thirtyfour::{
    FirefoxCapabilities, WebDriver, common::capabilities::firefox::FirefoxPreferences,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader as AsyncBufReader},
    process::Command as AsyncCommand,
    time::sleep,
};

// ─── Config ───────────────────────────────────────────────────────────────────

const GECKODRIVER_PORT: u16 = 4445;
const SECS_DAY: u64 = 86_400;
const SECS_MONTH: u64 = 86_400 * 30;

// ─── Portable paths ───────────────────────────────────────────────────────────

fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Cannot resolve executable path")?;
    exe.parent()
        .context("Executable has no parent directory")
        .map(|p| p.to_path_buf())
}

fn music_dir() -> Result<PathBuf> {
    let profile = std::env::var("USERPROFILE").context("USERPROFILE env var not set")?;
    Ok(PathBuf::from(profile).join("Music"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Versions cache (versions.json next to the exe) ──────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    geckodriver_version: Option<String>,
    ytdlp_last_check: Option<u64>,
    ffmpeg_last_dl: Option<u64>,
    deno_last_check: Option<u64>,
}

fn load_cache(path: &Path) -> Cache {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(path: &Path, c: &Cache) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(c)?)?;
    Ok(())
}

// ─── GitHub release helpers ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

async fn gh_latest(client: &Client, repo: &str) -> Result<GhRelease> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    client
        .get(&url)
        .header("User-Agent", "ytmusic-dl/1.0")
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GhRelease>()
        .await
        .context("Failed to parse GitHub release JSON")
}

fn strip_v(tag: &str) -> String {
    tag.trim_start_matches('v').to_string()
}

/// Returns true when semver string `a` is strictly older than `b`.
fn semver_older(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    parse(a) < parse(b)
}

// ─── Download helpers ─────────────────────────────────────────────────────────

/// Downloads `url` with a spinner, returns raw bytes.
async fn fetch_with_spinner(client: &Client, url: &str, label: &str) -> Result<Bytes> {
    let pb = spinner(&format!("{} …", label));
    let bytes = client
        .get(url)
        .header("User-Agent", "ytmusic-dl/1.0")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await
        .with_context(|| format!("Download failed: {}", url))?;
    pb.finish_and_clear();
    Ok(bytes)
}

/// Downloads `url` and writes it directly to `dest`.
async fn download_file(client: &Client, url: &str, dest: &Path) -> Result<()> {
    let label = dest.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let bytes = fetch_with_spinner(client, url, &format!("Downloading {}", label)).await?;
    fs::write(dest, &bytes).with_context(|| format!("Cannot write {}", dest.display()))?;
    Ok(())
}

/// Downloads a zip from `url`, extracts entries whose *filename* (not full path)
/// appears in `keep`. Pass an empty slice to extract everything.
async fn download_zip(
    client: &Client,
    url: &str,
    label: &str,
    dest: &Path,
    keep: &[&str],
) -> Result<()> {
    let bytes = fetch_with_spinner(client, url, label).await?;
    let cursor = io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open zip archive")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }

        let raw_name = entry.name().to_string();
        let fname = Path::new(&raw_name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        if fname.is_empty() {
            continue;
        }

        let want = keep.is_empty() || keep.iter().any(|&k| k == fname);
        if !want {
            continue;
        }

        let out = dest.join(&fname);
        let mut outf =
            fs::File::create(&out).with_context(|| format!("Cannot create {}", out.display()))?;
        io::copy(&mut entry, &mut outf)?;
    }
    Ok(())
}

/// Runs `bin --flag` and returns the word at `idx` on the first output line,
/// or `None` if the binary does not exist / exits with error.
fn binary_version(bin: &Path, flag: &str, idx: usize) -> Option<String> {
    if !bin.exists() {
        return None;
    }
    std::process::Command::new(bin)
        .arg(flag)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .as_deref()
        .and_then(|s| {
            s.lines()
                .next()?
                .split_whitespace()
                .nth(idx)
                .map(|w| w.to_string())
        })
}

// ─── Updaters ─────────────────────────────────────────────────────────────────

async fn update_geckodriver(client: &Client, dir: &Path, cache: &mut Cache) -> Result<()> {
    section("geckodriver");
    let bin = dir.join("geckodriver.exe");
    let rel = gh_latest(client, "mozilla/geckodriver").await?;
    let latest = strip_v(&rel.tag_name);

    let current = binary_version(&bin, "--version", 1);
    let needs = current
        .as_deref()
        .map(|v| semver_older(v, &latest))
        .unwrap_or(true);

    match &current {
        None => println!("  not found — will download {}", latest),
        Some(v) if needs => println!("  {} → {}", v, latest),
        Some(v) => {
            println!("  {} ✓", v);
            return Ok(());
        }
    }

    let zip_name = format!("geckodriver-v{}-win64.zip", latest);
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == zip_name)
        .with_context(|| format!("Asset '{}' not found in release", zip_name))?;

    download_zip(
        client,
        &asset.browser_download_url,
        "Downloading geckodriver",
        dir,
        &["geckodriver.exe"],
    )
    .await?;

    cache.geckodriver_version = Some(latest.clone());
    println!("  installed geckodriver {}", latest);
    Ok(())
}

async fn update_ytdlp(client: &Client, dir: &Path, cache: &mut Cache) -> Result<()> {
    let bin = dir.join("yt-dlp.exe");
    let now = now_secs();

    if bin.exists()
        && cache
            .ytdlp_last_check
            .map(|t| now - t < SECS_DAY)
            .unwrap_or(false)
    {
        section("yt-dlp");
        println!("  checked recently — skipping");
        return Ok(());
    }

    section("yt-dlp");
    let rel = gh_latest(client, "yt-dlp/yt-dlp").await?;
    let latest = strip_v(&rel.tag_name);
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == "yt-dlp.exe")
        .context("yt-dlp.exe not in release assets")?;

    // yt-dlp --version outputs the version string on stdout (e.g. 2024.12.13)
    let current = binary_version(&bin, "--version", 0);
    let needs = current
        .as_deref()
        .map(|v| v != latest.as_str())
        .unwrap_or(true);

    match &current {
        None => println!("  not found — will download {}", latest),
        Some(v) if needs => println!("  {} → {}", v, latest),
        Some(v) => {
            println!("  {} ✓", v);
            cache.ytdlp_last_check = Some(now);
            return Ok(());
        }
    }

    download_file(client, &asset.browser_download_url, &bin).await?;
    cache.ytdlp_last_check = Some(now);
    println!("  installed yt-dlp {}", latest);
    Ok(())
}

async fn update_ffmpeg(client: &Client, dir: &Path, cache: &mut Cache) -> Result<()> {
    let ffdir = dir.join("ffmpeg");
    let ffbin = ffdir.join("ffmpeg.exe");
    let now = now_secs();

    section("ffmpeg");
    if ffbin.exists()
        && cache
            .ffmpeg_last_dl
            .map(|t| now - t < SECS_MONTH)
            .unwrap_or(false)
    {
        println!("  updated recently — skipping");
        return Ok(());
    }

    fs::create_dir_all(&ffdir)?;

    let rel = gh_latest(client, "yt-dlp/FFmpeg-Builds").await?;
    let zip = "ffmpeg-master-latest-win64-gpl.zip";
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == zip)
        .context("ffmpeg zip not in release assets")?;

    // The zip layout is:  ffmpeg-master-latest-win64-gpl/bin/{ffmpeg,ffprobe}.exe
    // We stream-extract only those two binaries.
    let bytes = fetch_with_spinner(
        client,
        &asset.browser_download_url,
        "Downloading ffmpeg (large, ~100 MB)",
    )
    .await?;

    let cursor = io::Cursor::new(bytes);
    let mut arc = zip::ZipArchive::new(cursor)?;
    let mut extracted = 0u8;

    for i in 0..arc.len() {
        let mut f = arc.by_index(i)?;
        let raw = f.name().to_string();
        let is_ff = raw.ends_with("/bin/ffmpeg.exe");
        let is_fp = raw.ends_with("/bin/ffprobe.exe");
        if !is_ff && !is_fp {
            continue;
        }

        let fname = Path::new(&raw).file_name().unwrap().to_str().unwrap();
        let out = ffdir.join(fname);
        let mut outf =
            fs::File::create(&out).with_context(|| format!("Cannot create {}", out.display()))?;
        io::copy(&mut f, &mut outf)?;
        extracted += 1;
        if extracted == 2 {
            break;
        }
    }

    cache.ffmpeg_last_dl = Some(now);
    println!("  ffmpeg/ffprobe updated");
    Ok(())
}

async fn update_deno(client: &Client, dir: &Path, cache: &mut Cache) -> Result<()> {
    let bin = dir.join("deno.exe");
    let now = now_secs();

    if bin.exists()
        && cache
            .deno_last_check
            .map(|t| now - t < SECS_DAY)
            .unwrap_or(false)
    {
        section("deno");
        println!("  checked recently — skipping");
        return Ok(());
    }

    section("deno");
    let rel = gh_latest(client, "denoland/deno").await?;
    let latest = strip_v(&rel.tag_name);
    let zip = "deno-x86_64-pc-windows-msvc.zip";
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == zip)
        .context("deno zip not in release assets")?;

    // deno --version first line: "deno 2.x.x ..."
    let current = binary_version(&bin, "--version", 1);
    let needs = current
        .as_deref()
        .map(|v| semver_older(v, &latest))
        .unwrap_or(true);

    match &current {
        None => println!("  not found — will download {}", latest),
        Some(v) if needs => println!("  {} → {}", v, latest),
        Some(v) => {
            println!("  {} ✓", v);
            cache.deno_last_check = Some(now);
            return Ok(());
        }
    }

    download_zip(
        client,
        &asset.browser_download_url,
        "Downloading deno",
        dir,
        &["deno.exe"],
    )
    .await?;

    cache.deno_last_check = Some(now);
    println!("  installed deno {}", latest);
    Ok(())
}

// ─── Geckodriver process ──────────────────────────────────────────────────────

fn spawn_geckodriver(dir: &Path) -> Result<std::process::Child> {
    std::process::Command::new(dir.join("geckodriver.exe"))
        .args(["--port", &GECKODRIVER_PORT.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn geckodriver.exe")
}

// ─── WebDriver / Firefox ──────────────────────────────────────────────────────

async fn open_browser(dir: &Path) -> Result<WebDriver> {
    let profile = dir.join("firefox-profile");
    fs::create_dir_all(&profile)?;
    let profile_str = profile
        .to_str()
        .context("Profile path is not valid UTF-8")?;

    let mut caps = FirefoxCapabilities::new();
    // Pass profile path via Firefox command-line args (geckodriver supports this)
    caps.add_arg("-profile")?;
    caps.add_arg(profile_str)?;

    WebDriver::new(&format!("http://localhost:{}", GECKODRIVER_PORT), caps)
        .await
        .context("Failed to connect to Firefox via geckodriver")
}

// ─── Download-button injection ────────────────────────────────────────────────

/// JavaScript injected into every YouTube /watch page.
/// Returns: "exists" | "no_title" | "injected"
const INJECT_JS: &str = r#"
(function() {
    if (document.getElementById('__ytdl_btn')) return 'exists';

    var titleEl = document.querySelector('#above-the-fold > div:nth-child(1)');
    if (!titleEl) return 'no_title';

    var btn = document.createElement('button');
    btn.id = '__ytdl_btn';
    btn.textContent = '\u25BC\u00A0Download MP3';

    var s = btn.style;
    s.background     = 'linear-gradient(135deg, #b80000, #ff2222)';
    s.color          = '#ffffff';
    s.border         = 'none';
    s.padding        = '7px 18px';
    s.fontSize       = '13px';
    s.fontWeight     = '700';
    s.fontFamily     = '"YouTube Sans", Roboto, Arial, sans-serif';
    s.borderRadius   = '18px';
    s.cursor         = 'pointer';
    s.marginLeft     = '14px';
    s.verticalAlign  = 'middle';
    s.display        = 'inline-block';
    s.boxShadow      = '0 2px 8px rgba(0,0,0,.35)';
    s.transition     = 'opacity .15s ease';
    s.whiteSpace     = 'nowrap';
    s.letterSpacing  = '.3px';

    btn.onmouseover = function() { this.style.opacity = '.80'; };
    btn.onmouseout  = function() { this.style.opacity = '1';   };

    btn.addEventListener('click', function () {
        window.__ytdl_clicked = true;
        window.__ytdl_url     = location.href;
        this.textContent      = '\u2713\u00A0Starting\u2026';
        this.disabled         = true;
        this.style.background = '#555';
        this.style.cursor     = 'default';
        this.style.opacity    = '1';
    });

    titleEl.appendChild(btn);
    return 'injected';
})()
"#;

// ─── Video-ID extraction ──────────────────────────────────────────────────────

fn extract_video_id(url: &str) -> Option<String> {
    let qs = url.splitn(2, '?').nth(1).unwrap_or("");
    for pair in qs.split('&') {
        if let Some(val) = pair.strip_prefix("v=") {
            let id: String = val
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

// ─── yt-dlp download with progress ───────────────────────────────────────────

fn parse_pct(line: &str) -> Option<u64> {
    let pct = line.find('%')?;
    let before = &line[..pct];
    let start = before
        .rfind(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|i| i + 1)
        .unwrap_or(0);
    before[start..].trim().parse::<f64>().ok().map(|f| f as u64)
}

async fn run_download(dir: &Path, video_id: &str) -> Result<()> {
    let ytdlp = dir.join("yt-dlp.exe");
    let ffmpeg_dir = dir.join("ffmpeg");
    let deno_bin = dir.join("deno.exe");
    let out_dir = music_dir()?;

    let url = format!("https://www.youtube.com/watch?v={}", video_id);

    println!();
    println!("  Video : {}", url);
    println!("  Output: {}", out_dir.display());
    println!();

    // Build yt-dlp argument list
    let mut args: Vec<String> = vec![
        "--extract-audio".into(),
        "--audio-format".into(),
        "mp3".into(),
        "--audio-quality".into(),
        "0".into(), // 0 = best VBR
        "--no-playlist".into(),
        "--ffmpeg-location".into(),
        ffmpeg_dir.to_str().unwrap().into(),
        "--output".into(),
        "%(title)s.%(ext)s".into(),
        "--paths".into(),
        out_dir.to_str().unwrap().into(),
        "--newline".into(), // one progress line per \n (easier to parse)
        "--no-color".into(),
        "--progress".into(),
    ];

    // Use our portable deno if available
    if deno_bin.exists() {
        args.push("--no-js-runtimes".into());
        args.push("--js-runtimes".into());
        args.push(format!("deno:{}", deno_bin.to_str().unwrap()));
    }

    args.push(url);

    // Progress bar (0–100 maps directly to percentage)
    let pb = ProgressBar::new(100);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:52.red/white}] {pos:>3}%  {msg}")
            .unwrap()
            .progress_chars("█▊░"),
    );
    pb.set_message("Fetching info…");

    let mut child = AsyncCommand::new(&ytdlp)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start yt-dlp.exe")?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Print stderr lines above the bar (errors / warnings from yt-dlp)
    let pb2 = pb.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = AsyncBufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.contains("[download]") {
                pb2.println(format!("  {}", line.trim()));
            }
        }
    });

    let mut stdout_lines = AsyncBufReader::new(stdout).lines();
    while let Ok(Some(line)) = stdout_lines.next_line().await {
        if line.contains("[download]") && line.contains('%') {
            let pct = parse_pct(&line).unwrap_or(0).min(100);
            pb.set_position(pct);

            // Show "of X.XXMiB at Y.YYMiB/s ETA HH:MM" after the percentage
            if let Some(rest) = line.splitn(2, '%').nth(1) {
                let info = rest.trim().to_string();
                if !info.is_empty() {
                    pb.set_message(info);
                }
            }
        } else if line.contains("[ExtractAudio]") {
            pb.set_position(100);
            pb.set_message("Converting to MP3…");
        } else if line.contains("[Metadata]") {
            pb.set_message("Writing metadata…");
        } else if line.contains("[EmbedThumbnail]") {
            pb.set_message("Embedding thumbnail…");
        } else if !line.trim().is_empty() && line.starts_with('[') {
            pb.println(format!("  {}", line.trim()));
        }
    }

    let _ = stderr_task.await;
    let status = child.wait().await.context("yt-dlp process error")?;

    if status.success() {
        pb.finish_with_message("✓ Done!");
        println!();
        println!("  Saved to {}", out_dir.display());
    } else {
        pb.finish_with_message("✗ Failed");
        return Err(anyhow!("yt-dlp exited with non-zero status"));
    }

    Ok(())
}

// ─── Main loop ────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let dir = exe_dir()?;
    let cache_path = dir.join("versions.json");

    print_banner();

    // Ensure sub-directories exist
    fs::create_dir_all(dir.join("ffmpeg"))?;
    fs::create_dir_all(dir.join("firefox-profile"))?;

    let mut cache = load_cache(&cache_path);

    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;

    // ── Dependency updates ────────────────────────────────────────────────────
    println!("Dependencies");
    println!("────────────────────────────────────────");

    macro_rules! try_update {
        ($fut:expr, $name:expr) => {
            if let Err(e) = $fut.await {
                eprintln!("  WARNING: {} update failed: {}", $name, e);
            }
        };
    }

    try_update!(update_geckodriver(&client, &dir, &mut cache), "geckodriver");
    try_update!(update_ytdlp(&client, &dir, &mut cache), "yt-dlp");
    try_update!(update_ffmpeg(&client, &dir, &mut cache), "ffmpeg");
    try_update!(update_deno(&client, &dir, &mut cache), "deno");

    save_cache(&cache_path, &cache)?;

    // ── Browser ───────────────────────────────────────────────────────────────
    println!();
    println!("Browser");
    println!("────────────────────────────────────────");

    let mut geckoproc = spawn_geckodriver(&dir)?;
    sleep(Duration::from_millis(2000)).await;

    let driver = match open_browser(&dir).await {
        Ok(d) => d,
        Err(e) => {
            let _ = geckoproc.kill();
            return Err(e);
        }
    };

    driver
        .get("https://www.youtube.com")
        .await
        .context("Could not navigate to YouTube")?;

    println!("  Firefox is ready.");
    println!("  Navigate to a YouTube video, then click ▼ Download MP3.");
    println!();

    // ── Poll loop ─────────────────────────────────────────────────────────────
    let mut last_url = String::new();
    let mut download_url: Option<String> = None;

    'poll: loop {
        sleep(Duration::from_millis(600)).await;

        // Detect if the browser was closed by the user
        let current_url = match driver.current_url().await {
            Ok(u) => u.to_string(),
            Err(_) => {
                println!("  Browser was closed — exiting.");
                break 'poll;
            }
        };

        if !current_url.contains("youtube.com/watch") {
            if current_url != last_url {
                last_url = current_url;
            }
            continue;
        }

        // ── We are on a /watch page ───────────────────────────────────────────
        if current_url != last_url {
            // URL changed (new video navigated to): reset click state and
            // wait for YouTube's SPA to finish rendering the new page.
            last_url = current_url.clone();
            let _ = driver
                .execute(
                    "window.__ytdl_clicked = false; window.__ytdl_url = '';",
                    vec![],
                )
                .await;
            sleep(Duration::from_millis(2500)).await;
        }

        // Inject (or re-inject) the button.
        // The script is idempotent: it returns "exists" if already present.
        if let Ok(r) = driver.execute(INJECT_JS, vec![]).await {
            if r.json().as_str() == Some("injected") {
                // New injection — print the video title for feedback
                if let Ok(t) = driver
                    .execute(
                        "return document.title.replace(/ - YouTube$/, '').trim();",
                        vec![],
                    )
                    .await
                {
                    let title = t.json().as_str().unwrap_or("?");
                    println!("  ▶  \"{}\"", title);
                }
            }
        }

        // Check if the user clicked Download
        let clicked = driver
            .execute("return window.__ytdl_clicked === true;", vec![])
            .await
            .map(|r| r.json().as_bool().unwrap_or(false))
            .unwrap_or(false);

        if clicked {
            let url = driver
                .execute("return window.__ytdl_url || '';", vec![])
                .await
                .map(|r| r.json().as_str().unwrap_or("").to_string())
                .unwrap_or_default();

            if !url.is_empty() {
                download_url = Some(url);
                break 'poll;
            }
        }
    }

    // ── Tear down browser ─────────────────────────────────────────────────────
    let _ = driver.quit().await;
    let _ = geckoproc.kill();

    // ── Download ──────────────────────────────────────────────────────────────
    match download_url.and_then(|u| extract_video_id(&u).map(|id| (u, id))) {
        None => {
            eprintln!("No video URL captured.");
        }
        Some((raw_url, video_id)) => {
            println!();
            println!("Download");
            println!("────────────────────────────────────────");
            println!("  ID: {}", video_id);
            if let Err(e) = run_download(&dir, &video_id).await {
                eprintln!("  Error: {}", e);
                eprintln!("  URL was: {}", raw_url);
            }
        }
    }

    println!();
    println!("────────────────────────────────────────");
    print!("Press Enter to close…");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    Ok(())
}

// ─── UI helpers ───────────────────────────────────────────────────────────────

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::with_template("{spinner:.cyan}  {msg}").unwrap());
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn section(name: &str) {
    println!("  [{}]", name);
}

fn print_banner() {
    println!();
    println!("  ╔═══════════════════════════════════════╗");
    println!("  ║     YouTube Music Downloader (ytmd)   ║");
    println!("  ╚═══════════════════════════════════════╝");
    println!();
}
