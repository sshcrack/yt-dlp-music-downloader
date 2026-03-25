// ytmusic-dl — portable YouTube → MP3 downloader
// A Firefox extension injects a download button on YouTube /watch pages.
// Clicking it opens the custom URI  ytdlpmusic://<video_id>  which launches
// this binary to download the video as MP3 via yt-dlp.
//
// Two modes:
//   setup mode  (no arguments)  — update bundled tools, register the URI
//                                 scheme, and print extension install info.
//   download mode  (ytdlpmusic://VIDEO_ID)  — download the MP3 directly.

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
use sysuri::UriScheme;
use tokio::{
    io::{AsyncBufReadExt, BufReader as AsyncBufReader},
    process::Command as AsyncCommand,
};

// ─── Config ───────────────────────────────────────────────────────────────────

const SECS_DAY: u64 = 86_400;
const SECS_MONTH: u64 = 86_400 * 30;

// ─── Portable paths ───────────────────────────────────────────────────────────

fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Kann ausführbaren Pfad nicht ermitteln")?;
    exe.parent()
        .context("Die ausführbare Datei hat kein übergeordnetes Verzeichnis")
        .map(|p| p.to_path_buf())
}

fn music_dir() -> Result<PathBuf> {
    let profile =
        std::env::var("USERPROFILE").context("Umgebungsvariable USERPROFILE nicht gesetzt")?;
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
        .context("Konnte GitHub-Release-JSON nicht parsen")
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
        .bytes()
        .await
        .with_context(|| format!("Herunterladen fehlgeschlagen: {}", url))?;
    pb.finish_and_clear();
    Ok(bytes)
}

/// Downloads `url` and writes it directly to `dest`.
async fn download_file(client: &Client, url: &str, dest: &Path) -> Result<()> {
    let label = dest.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let bytes = fetch_with_spinner(client, url, &format!("Lade {} herunter", label)).await?;
    fs::write(dest, &bytes).with_context(|| format!("Kann nicht schreiben: {}", dest.display()))?;
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
    let mut archive = zip::ZipArchive::new(cursor).context("Konnte Zip-Archiv nicht öffnen")?;

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
        let mut outf = fs::File::create(&out)
            .with_context(|| format!("Kann nicht erstellen: {}", out.display()))?;
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
        println!("  kürzlich geprüft — überspringe");
        return Ok(());
    }

    section("yt-dlp");
    let rel = gh_latest(client, "yt-dlp/yt-dlp").await?;
    let latest = strip_v(&rel.tag_name);
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == "yt-dlp.exe")
        .context("yt-dlp.exe nicht in Release-Assets")?;

    // yt-dlp --version outputs the version string on stdout (e.g. 2024.12.13)
    let current = binary_version(&bin, "--version", 0);
    let needs = current
        .as_deref()
        .map(|v| v != latest.as_str())
        .unwrap_or(true);

    match &current {
        None => println!("  nicht gefunden — lade {} herunter", latest),
        Some(v) if needs => println!("  {} → {}", v, latest),
        Some(v) => {
            println!("  {} ✓", v);
            cache.ytdlp_last_check = Some(now);
            return Ok(());
        }
    }

    download_file(client, &asset.browser_download_url, &bin).await?;
    cache.ytdlp_last_check = Some(now);
    println!("  yt-dlp {} installiert", latest);
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
        println!("  vor Kurzem aktualisiert — überspringe");
        return Ok(());
    }

    fs::create_dir_all(&ffdir)?;

    let rel = gh_latest(client, "yt-dlp/FFmpeg-Builds").await?;
    let zip = "ffmpeg-master-latest-win64-gpl.zip";
    let asset = rel
        .assets
        .iter()
        .find(|a| a.name == zip)
        .context("ffmpeg zip nicht in Release-Assets")?;

    // The zip layout is:  ffmpeg-master-latest-win64-gpl/bin/{ffmpeg,ffprobe}.exe
    // We stream-extract only those two binaries.
    let bytes = fetch_with_spinner(
        client,
        &asset.browser_download_url,
        "Lade ffmpeg herunter (groß, ~100 MB)",
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
        let mut outf = fs::File::create(&out)
            .with_context(|| format!("Kann nicht erstellen: {}", out.display()))?;
        io::copy(&mut f, &mut outf)?;
        extracted += 1;
        if extracted == 2 {
            break;
        }
    }

    cache.ffmpeg_last_dl = Some(now);
    println!("  ffmpeg/ffprobe aktualisiert");
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
        println!("  kürzlich geprüft — überspringe");
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
        .context("deno zip nicht in Release-Assets")?;

    // deno --version first line: "deno 2.x.x ..."
    let current = binary_version(&bin, "--version", 1);
    let needs = current
        .as_deref()
        .map(|v| semver_older(v, &latest))
        .unwrap_or(true);

    match &current {
        None => println!("  nicht gefunden — lade {} herunter", latest),
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
        "Lade deno herunter",
        dir,
        &["deno.exe"],
    )
    .await?;

    cache.deno_last_check = Some(now);
    println!("  deno {} installiert", latest);
    Ok(())
}

// ─── Video-ID extraction ──────────────────────────────────────────────────────

/// Accepts either a full YouTube URL or a bare video ID (from the custom URI).
fn extract_video_id(input: &str) -> Option<String> {
    // ytdlpmusic://VIDEO_ID  — the authority part is the ID
    if let Some(id) = input.strip_prefix("ytdlpmusic://") {
        let id = id.trim_matches('/');
        let id: String = id
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !id.is_empty() {
            return Some(id);
        }
    }

    // Full YouTube URL  (https://www.youtube.com/watch?v=…)
    let qs = input.splitn(2, '?').nth(1).unwrap_or("");
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

async fn run_download(dir: &Path, video_id: &str) -> Result<Option<String>> {
    let ytdlp = dir.join("yt-dlp.exe");
    let ffmpeg_dir = dir.join("ffmpeg");
    let deno_bin = dir.join("deno.exe");
    let out_dir = music_dir()?;

    let url = format!("https://www.youtube.com/watch?v={}", video_id);

    println!();
    println!("  Video : {}", url);
    println!("  Ausgabe: {}", out_dir.display());
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
    pb.set_message("Informationen werden abgerufen…");

    let mut child = AsyncCommand::new(&ytdlp)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Starten von yt-dlp.exe fehlgeschlagen")?;

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
            pb.set_message("Wandle in MP3 um…");
        } else if line.contains("[Metadata]") {
            pb.set_message("Schreibe Metadaten…");
        } else if line.contains("[EmbedThumbnail]") {
            pb.set_message("Vorschaubild einbetten…");
        } else if !line.trim().is_empty() && line.starts_with('[') {
            pb.println(format!("  {}", line.trim()));
        }
    }

    let _ = stderr_task.await;
    let status = child.wait().await.context("yt-dlp Prozessfehler")?;

    if status.success() {
        pb.finish_with_message("✓ Fertig!");
        println!();
        println!("  Gespeichert in {}", out_dir.display());

        // Versuche, die zuletzt erstellte MP3-Datei im Ausgabeverzeichnis zu finden.
        let newest = std::fs::read_dir(&out_dir)
            .ok()
            .into_iter()
            .flat_map(|rd| rd.filter_map(Result::ok))
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("mp3"))
                    .unwrap_or(true)
            })
            .max_by_key(|p| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });

        if let Some(p) = newest {
            return Ok(Some(p.display().to_string()));
        }

        return Ok(None);
    } else {
        pb.finish_with_message("✗ Fehlgeschlagen");
        return Err(anyhow!("yt-dlp mit Fehlercode beendet"));
    }
}

// ─── Windows URI-scheme registration ─────────────────────────────────────────

fn register_uri_scheme() -> Result<()> {
    let exe = std::env::current_exe().context("Kann ausführbaren Pfad nicht ermitteln")?;
    let scheme = UriScheme::new("ytdlpmusic", "Downloads youtube videos as music", exe);

    sysuri::register(&scheme)?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn register_uri_scheme() -> Result<()> {
    Err(anyhow!(
        "URI-Schema-Registrierung wird nur unter Windows unterstützt"
    ))
}

// ─── Main loop ────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Download mode: launched by Windows as the handler for ytdlpmusic://…
    if let Some(uri) = sysuri::parse_args() {
        return download_mode(&uri).await;
    }

    // Setup mode: first run / manual launch
    setup_mode().await
}

/// Setup mode — update bundled tools, register the URI scheme, print help.
async fn setup_mode() -> Result<()> {
    let dir = exe_dir()?;
    let cache_path = dir.join("versions.json");

    print_banner();

    fs::create_dir_all(dir.join("ffmpeg"))?;

    let mut cache = load_cache(&cache_path);

    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;

    // ── Dependency updates ────────────────────────────────────────────────────
    println!("Abhängigkeiten");
    println!("────────────────────────────────────────");

    macro_rules! try_update {
        ($fut:expr, $name:expr) => {
            if let Err(e) = $fut.await {
                eprintln!(
                    "  WARNUNG: Aktualisierung von {} fehlgeschlagen: {}",
                    $name, e
                );
            }
        };
    }

    try_update!(update_ytdlp(&client, &dir, &mut cache), "yt-dlp");
    try_update!(update_ffmpeg(&client, &dir, &mut cache), "ffmpeg");
    try_update!(update_deno(&client, &dir, &mut cache), "deno");

    save_cache(&cache_path, &cache)?;

    // ── URI scheme registration ───────────────────────────────────────────────
    println!();
    println!("URI-Schema");
    println!("────────────────────────────────────────");
    match register_uri_scheme() {
        Ok(()) => println!("  ytdlpmusic:// erfolgreich registriert ✓"),
        Err(e) => eprintln!("  WARNUNG: Registrierung fehlgeschlagen: {}", e),
    }

    // ── Extension install instructions ───────────────────────────────────────
    println!();
    println!("Firefox-Erweiterung");
    println!("────────────────────────────────────────");
    println!("  Lade die mitgelieferte Erweiterung im Ordner 'extension/' in Firefox:");
    println!("    1. Firefox öffnen");
    println!("    2. about:debugging aufrufen");
    println!("    3. \"Dieser Firefox\" → \"Temporäres Add-on laden\"");
    println!("    4. extension/manifest.json auswählen");
    println!();
    println!("  Für eine dauerhafte Installation: Extension als .zip verpacken");
    println!("  und bei addons.mozilla.org einreichen.");
    println!();
    println!("────────────────────────────────────────");
    print!("Drücke Enter zum Schließen…");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    Ok(())
}

/// Download mode — called by Windows when the user opens a ytdlpmusic:// URI.
async fn download_mode(uri: &str) -> Result<()> {
    let dir = exe_dir()?;
    let cache_path = dir.join("versions.json");

    print_banner();

    // extract_video_id strips the URI to only alphanumeric / '-' / '_' chars,
    // so no shell-injection is possible even with a crafted ytdlpmusic:// URI.
    let video_id = extract_video_id(uri)
        .ok_or_else(|| anyhow!("Ungültige URI — kein Video-ID gefunden: {}", uri))?;

    let mut cache = load_cache(&cache_path);

    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;

    // Silently ensure tools are up-to-date (respects the day/month throttle).
    fs::create_dir_all(dir.join("ffmpeg"))?;

    macro_rules! try_update {
        ($fut:expr, $name:expr) => {
            if let Err(e) = $fut.await {
                eprintln!(
                    "  WARNUNG: Aktualisierung von {} fehlgeschlagen: {}",
                    $name, e
                );
            }
        };
    }

    println!("Abhängigkeiten");
    println!("────────────────────────────────────────");
    try_update!(update_ytdlp(&client, &dir, &mut cache), "yt-dlp");
    try_update!(update_ffmpeg(&client, &dir, &mut cache), "ffmpeg");
    try_update!(update_deno(&client, &dir, &mut cache), "deno");
    save_cache(&cache_path, &cache)?;

    println!();
    println!("Herunterladen");
    println!("────────────────────────────────────────");
    println!("  ID: {}", video_id);

    match run_download(&dir, &video_id).await {
        Ok(Some(created)) => {
            println!();
            print_completion(&created);
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("  Fehler: {}", e);
        }
    }

    println!();
    println!("────────────────────────────────────────");
    print!("Drücke Enter zum Schließen…");
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

fn print_completion(path: &str) {
    // Make the completion very hard to miss: several large boxed banners
    let width = 78usize;
    let bar = "#".repeat(width);

    // A few blank lines to separate from previous output
    println!(
        "

"
    );

    for _ in 0..3 {
        println!("{}", bar);
        println!("#{: ^width$}#", "", width = width - 2);
        println!(
            "#{: ^width$}#",
            "✓   FERTIG!   —   DOWNLOADED",
            width = width - 2
        );
        println!("#{: ^width$}#", "", width = width - 2);
        println!("{}", bar);
        println!();
    }

    println!("  Ausgabedatei:");
    println!("  {}", path);
    println!();
}
