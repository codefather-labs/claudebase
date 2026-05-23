//! Voice transcription via `whisper-cli` subprocess. Ports the TSX
//! patched-fork module that adds whisper.cpp support
//! (codefather-labs/claude-plugins-official server.ts:913-1198) to the
//! upstream Anthropic plugin. Skips the auto-install path: if the
//! binaries aren't present we log clear remediation hints and return
//! None so the voice handler falls back to a placeholder.

use frankenstein::client_reqwest::Bot;
use frankenstein::methods::GetFileParams;
use frankenstein::AsyncTelegramApi;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(30);
const WHISPER_TIMEOUT: Duration = Duration::from_secs(120);

struct WhisperEnv {
    ffmpeg: PathBuf,
    whisper_cli: PathBuf,
    model: PathBuf,
}

static WHISPER_ENV: OnceLock<Option<WhisperEnv>> = OnceLock::new();

/// Resolve ffmpeg + whisper-cli + model paths. Cached after first call.
/// Returns None on first failure (binaries or model not found) — caller
/// falls back to placeholder. Mirrors TSX `ensureWhisper`.
fn resolve_env() -> Option<&'static WhisperEnv> {
    WHISPER_ENV
        .get_or_init(|| {
            let ffmpeg = find_binary_with_override("FFMPEG_PATH", ffmpeg_bin_name())?;
            let whisper_cli =
                find_binary_with_override("WHISPER_CLI_PATH", whisper_bin_name())?;
            let model = resolve_model_path()?;
            tracing::info!(
                ffmpeg = ?ffmpeg,
                whisper_cli = ?whisper_cli,
                model = ?model,
                "whisper transcription enabled"
            );
            Some(WhisperEnv {
                ffmpeg,
                whisper_cli,
                model,
            })
        })
        .as_ref()
}

fn ffmpeg_bin_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

fn whisper_bin_name() -> &'static str {
    if cfg!(windows) { "whisper-cli.exe" } else { "whisper-cli" }
}

/// Find a binary. Env-var override wins; otherwise scan a small list of
/// common install paths. Returns None if not found (caller logs hint).
fn find_binary_with_override(env_var: &str, name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(env = env_var, ?path, "override path is not a regular file — ignoring");
    }
    for dir in search_paths() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    tracing::warn!(
        binary = name,
        "not found in PATH search — install via package manager (brew install whisper-cpp / ffmpeg on macOS) to enable transcription"
    );
    None
}

fn search_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut paths = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/snap/bin"),
        PathBuf::from(&home).join(".local/bin"),
    ];
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join("Programs/whisper-cpp/bin"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            paths.push(PathBuf::from(pf).join("whisper-cpp/bin"));
        }
        paths.push(PathBuf::from(&home).join("scoop/shims"));
    }
    paths
}

fn resolve_model_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("WHISPER_MODEL_PATH") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(?path, "WHISPER_MODEL_PATH does not exist");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = std::env::var("WHISPER_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".local/share/whisper-cpp/models"));
    let name =
        std::env::var("WHISPER_MODEL_NAME").unwrap_or_else(|_| "ggml-medium.bin".to_string());
    let path = dir.join(&name);
    if path.is_file() {
        return Some(path);
    }

    // Lazy download from HuggingFace. Blocking thread call from a sync
    // helper — model resolution happens once per process from a synchronous
    // OnceLock initializer, so we use a blocking reqwest client here.
    // Auto-download is OPT-IN via WHISPER_AUTO_DOWNLOAD=1 (large file, ~1.5 GB
    // for ggml-medium.bin; we don't want to silently burn a user's bandwidth).
    if std::env::var("WHISPER_AUTO_DOWNLOAD").as_deref() != Ok("1") {
        tracing::warn!(
            ?path,
            "whisper model not found — set WHISPER_AUTO_DOWNLOAD=1 or download manually from https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            name
        );
        return None;
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, ?dir, "create model dir failed");
        return None;
    }
    let url = std::env::var("WHISPER_MODEL_URL").unwrap_or_else(|_| {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            name
        )
    });
    let part_path = path.with_extension(format!(
        "{}.part",
        path.extension().and_then(|e| e.to_str()).unwrap_or("bin")
    ));
    tracing::info!(model = %name, url = %url, "WHISPER_AUTO_DOWNLOAD=1 — downloading whisper model (this may take several minutes)");

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1800)) // 30 min
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "reqwest client build failed");
            return None;
        }
    };
    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "model download request failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "model download non-success status");
        return None;
    }
    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "model body read failed");
            return None;
        }
    };
    if bytes.len() < 100_000_000 {
        tracing::warn!(size = bytes.len(), "model file too small — download may have failed");
        return None;
    }
    if let Err(e) = std::fs::write(&part_path, &bytes) {
        tracing::warn!(error = %e, ?part_path, "model write failed");
        return None;
    }
    if let Err(e) = std::fs::rename(&part_path, &path) {
        tracing::warn!(error = %e, ?path, "model rename failed");
        return None;
    }
    tracing::info!(
        size_mb = bytes.len() / 1024 / 1024,
        ?path,
        "whisper model downloaded"
    );
    Some(path)
}

/// Download a Telegram voice .oga, transcode to 16kHz mono PCM .wav via
/// ffmpeg, then run whisper-cli with `-l auto`. Returns Some(transcript)
/// or None on any failure (logged). Mirrors TSX `transcribeVoice`.
pub async fn transcribe_voice(bot: &Bot, token: &str, file_id: &str) -> Option<String> {
    let env = resolve_env()?;

    let inbox = crate::state::inbox_dir();
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        tracing::warn!(error = %e, "could not create inbox dir");
        return None;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let oga_path = inbox.join(format!("{}-voice.oga", ts));
    let wav_path = inbox.join(format!("{}-voice.wav", ts));

    // Cleanup guard — both files removed on any exit path.
    let _cleanup = CleanupGuard::new(&[&oga_path, &wav_path]);

    // --- Download .oga from Telegram CDN ---
    let params = GetFileParams::builder().file_id(file_id.to_string()).build();
    let file = match bot.get_file(&params).await {
        Ok(r) => r.result,
        Err(e) => {
            tracing::warn!(error = ?e, file_id = %file_id, "get_file failed for voice");
            return None;
        }
    };
    let Some(file_path) = file.file_path else {
        tracing::warn!(file_id = %file_id, "no file_path in get_file response (voice)");
        return None;
    };
    let url = format!("https://api.telegram.org/file/bot{}/{}", token, file_path);
    let bytes = match reqwest::get(&url).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "voice body read failed");
                return None;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "voice fetch failed");
            return None;
        }
    };
    if let Err(e) = std::fs::write(&oga_path, &bytes) {
        tracing::warn!(error = %e, "voice write failed");
        return None;
    }

    // --- ffmpeg: .oga → .wav (16kHz mono PCM s16le) ---
    let ff_result = run_cmd(
        &env.ffmpeg,
        &[
            "-y",
            "-i",
            oga_path.to_string_lossy().as_ref(),
            "-ar",
            "16000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            wav_path.to_string_lossy().as_ref(),
        ],
        FFMPEG_TIMEOUT,
    )
    .await;
    match ff_result {
        Ok((exit, _stdout, stderr)) if exit == 0 => {}
        Ok((exit, _, stderr)) => {
            tracing::warn!(exit = exit, stderr = %stderr, "ffmpeg conversion failed");
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "ffmpeg subprocess error");
            return None;
        }
    }

    // --- whisper-cli ---
    let wh_result = run_cmd(
        &env.whisper_cli,
        &[
            "-m",
            env.model.to_string_lossy().as_ref(),
            "-f",
            wav_path.to_string_lossy().as_ref(),
            "-nt", // no timestamps
            "-t",
            "4", // 4 threads
            "-l",
            "auto", // auto language detect
        ],
        WHISPER_TIMEOUT,
    )
    .await;
    let (exit, stdout, stderr) = match wh_result {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "whisper subprocess error");
            return None;
        }
    };
    if exit != 0 {
        tracing::warn!(exit = exit, stderr = %stderr, "whisper-cli failed");
        return None;
    }

    let transcript: String = stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    if transcript.is_empty() {
        tracing::warn!("whisper produced empty transcription");
        return None;
    }

    tracing::info!(chars = transcript.len(), "voice transcribed");
    Some(transcript)
}

/// Run `bin args...`, capture stdout+stderr, enforce timeout. Returns
/// (exit_code, stdout, stderr) on completion (whether success or
/// non-zero exit). Errs on timeout or spawn failure.
async fn run_cmd(
    bin: &PathBuf,
    args: &[&str],
    deadline: Duration,
) -> std::io::Result<(i32, String, String)> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd.spawn()?;

    match timeout(deadline, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let exit = output.status.code().unwrap_or(-1);
            Ok((exit, stdout, stderr))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("subprocess exceeded {}s timeout", deadline.as_secs()),
        )),
    }
}

/// Best-effort cleanup of intermediate files on scope exit.
struct CleanupGuard<'a> {
    paths: Vec<&'a PathBuf>,
}

impl<'a> CleanupGuard<'a> {
    fn new(paths: &[&'a PathBuf]) -> Self {
        CleanupGuard {
            paths: paths.to_vec(),
        }
    }
}

impl Drop for CleanupGuard<'_> {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}
