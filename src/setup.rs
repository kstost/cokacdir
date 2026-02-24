use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

// ── i18n strings ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum Lang {
    Ko,
    En,
}

struct Strings {
    header: &'static str,
    lang_prompt: &'static str,
    token_prompt: &'static str,
    token_invalid_format: &'static str,
    token_validation_skip: &'static str,
    token_saved: &'static str,
    workdir_prompt: &'static str,
    workdir_default: &'static str,
    workdir_not_dir: &'static str,
    git_ok: &'static str,
    git_missing: &'static str,
    git_install_hint: &'static str,
    gh_ok: &'static str,
    gh_missing: &'static str,
    gh_install_hint: &'static str,
    complete: &'static str,
    hint_run: &'static str,
    hint_setup: &'static str,
}

const KO: Strings = Strings {
    header: "cokacdir Telegram 봇 초기 설정",
    lang_prompt: "언어를 선택하세요 (1=한국어, 2=English) [기본값: 1]: ",
    token_prompt: "Telegram 봇 토큰을 입력하세요: ",
    token_invalid_format: "  ! 유효하지 않은 토큰 형식입니다. 형식: 123456:ABC... (최대 3회 시도)",
    token_validation_skip: "  ! 토큰 형식 검증을 건너뜁니다",
    token_saved: "  ✓ 설정이 저장되었습니다",
    workdir_prompt: "작업 디렉토리를 입력하세요",
    workdir_default: "기본값",
    workdir_not_dir: "  ! 디렉토리가 아닙니다. 홈 디렉토리를 사용합니다",
    git_ok: "  ✓ git 설치됨",
    git_missing: "  ! git이 설치되어 있지 않습니다",
    git_install_hint: "    설치: https://git-scm.com/downloads",
    gh_ok: "  ✓ gh (GitHub CLI) 설치됨",
    gh_missing: "  ! gh (GitHub CLI)가 설치되어 있지 않습니다",
    gh_install_hint: "    설치: https://cli.github.com",
    complete: "설정이 완료되었습니다!",
    hint_run: "봇 시작하기:",
    hint_setup: "다시 설정하려면:",
};

const EN: Strings = Strings {
    header: "cokacdir Telegram Bot Setup",
    lang_prompt: "Select language (1=Korean, 2=English) [default: 2]: ",
    token_prompt: "Enter your Telegram bot token: ",
    token_invalid_format: "  ! Invalid token format. Expected: 123456:ABC... (max 3 attempts)",
    token_validation_skip: "  ! Skipping token format validation",
    token_saved: "  ✓ Configuration saved",
    workdir_prompt: "Enter working directory",
    workdir_default: "default",
    workdir_not_dir: "  ! Not a directory. Using home directory",
    git_ok: "  ✓ git is installed",
    git_missing: "  ! git is not installed",
    git_install_hint: "    Install: https://git-scm.com/downloads",
    gh_ok: "  ✓ gh (GitHub CLI) is installed",
    gh_missing: "  ! gh (GitHub CLI) is not installed",
    gh_install_hint: "    Install: https://cli.github.com",
    complete: "Setup complete!",
    hint_run: "Start the bot:",
    hint_setup: "Run setup again:",
};

fn s(lang: Lang) -> &'static Strings {
    match lang {
        Lang::Ko => &KO,
        Lang::En => &EN,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn read_line_trimmed() -> String {
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return String::new();
    }
    input.trim().to_string()
}

fn find_in_path(name: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if std::path::Path::new(dir).join(name).exists() {
                return true;
            }
        }
    }
    false
}

fn get_cmd_output(name: &str, args: &[&str]) -> Option<String> {
    Command::new(name)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cokacdir").join("config.json"))
}

// ── Setup steps ──────────────────────────────────────────────────────────────

fn select_language() -> Lang {
    print!("{}", EN.lang_prompt);
    io::stdout().flush().ok();
    let input = read_line_trimmed();
    if input == "1" {
        Lang::Ko
    } else {
        Lang::En
    }
}

fn validate_token_format(token: &str) -> bool {
    // Telegram bot token format: <bot_id>:<auth_token>
    // bot_id: digits only, auth_token: alphanumeric + _ and -
    let parts: Vec<&str> = token.splitn(2, ':').collect();
    if parts.len() != 2 {
        return false;
    }
    let bot_id = parts[0];
    let auth = parts[1];
    !bot_id.is_empty()
        && bot_id.chars().all(|c| c.is_ascii_digit())
        && auth.len() >= 10
        && auth.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

fn get_telegram_token(lang: Lang) -> Result<String, Box<dyn std::error::Error>> {
    let t = s(lang);
    for attempt in 0..3 {
        print!("{}", t.token_prompt);
        io::stdout().flush()?;
        let token = read_line_trimmed();

        if token.is_empty() {
            if attempt == 2 {
                println!("{}", t.token_validation_skip);
                return Err("No token provided".into());
            }
            println!("{}", t.token_invalid_format);
            continue;
        }

        if validate_token_format(&token) {
            return Ok(token);
        }

        if attempt < 2 {
            println!("{}", t.token_invalid_format);
        } else {
            println!("{}", t.token_validation_skip);
            return Err("Invalid token format after 3 attempts".into());
        }
    }
    Err("Token input failed".into())
}

fn get_working_directory(lang: Lang) -> PathBuf {
    let t = s(lang);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    print!("{} [{}: {}]: ", t.workdir_prompt, t.workdir_default, home.display());
    io::stdout().flush().ok();
    let input = read_line_trimmed();

    if input.is_empty() {
        return home;
    }

    let path = PathBuf::from(&input);
    if path.is_dir() {
        path
    } else {
        println!("{}", t.workdir_not_dir);
        home
    }
}

fn save_setup_config(
    token: &str,
    lang: Lang,
    work_dir: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let lang_str = match lang {
        Lang::Ko => "ko",
        Lang::En => "en",
    };

    let config_json = serde_json::json!({
        "telegram_token": token,
        "language": lang_str,
        "working_directory": work_dir.to_string_lossy().as_ref(),
    });

    let path = config_path().ok_or("Cannot determine home directory")?;
    let dir = path.parent().ok_or("Cannot determine config directory")?;

    fs::create_dir_all(dir)?;

    // Set directory permissions to user-only on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let content = serde_json::to_string_pretty(&config_json)?;
    fs::write(&path, &content)?;

    // Set file permissions to user-only on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

fn check_git_installed(lang: Lang) {
    let t = s(lang);
    if find_in_path("git") {
        let ver = get_cmd_output("git", &["--version"]).unwrap_or_default();
        println!("{}: {}", t.git_ok, ver);
    } else {
        println!("{}", t.git_missing);
        println!("{}", t.git_install_hint);
    }
}

fn check_gh_installed(lang: Lang) {
    let t = s(lang);
    if find_in_path("gh") {
        let ver = get_cmd_output("gh", &["--version"]).unwrap_or_default();
        println!("{}: {}", t.gh_ok, ver);
    } else {
        println!("{}", t.gh_missing);
        println!("{}", t.gh_install_hint);
    }
}

fn print_success_message(lang: Lang, work_dir: &PathBuf) {
    let t = s(lang);
    let bar = "─".repeat(50);
    println!();
    println!("{bar}");
    println!("  {}", t.complete);
    println!();
    println!("  {}:", t.hint_run);
    println!("    cokacdir --ccserver");
    println!();
    println!("  {}:", t.hint_setup);
    println!("    cokacdir --setup");
    println!();
    println!("  Working directory: {}", work_dir.display());
    println!("{bar}");
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Run the interactive setup wizard for Telegram bot configuration
pub fn run_setup() -> Result<(), Box<dyn std::error::Error>> {
    let bar = "─".repeat(50);
    println!();
    println!("{bar}");
    println!("  cokacdir Telegram Bot Setup");
    println!("{bar}");
    println!();

    // Step 1: Language selection
    let lang = select_language();
    println!();

    // Step 2: Telegram bot token
    let token = get_telegram_token(lang)?;
    println!();

    // Step 3: Working directory
    let work_dir = get_working_directory(lang);
    println!();

    // Step 4: Save config
    save_setup_config(&token, lang, &work_dir)?;
    println!("{}", s(lang).token_saved);
    println!();

    // Step 5: Optional checks
    check_git_installed(lang);
    check_gh_installed(lang);

    // Step 6: Success message
    print_success_message(lang, &work_dir);

    Ok(())
}

/// Load the saved Telegram token from ~/.cokacdir/config.json
pub fn load_saved_token() -> Option<String> {
    let path = config_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("telegram_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}
