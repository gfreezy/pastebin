use pastebin::script::{MAX_SOURCE, ScriptExecutor};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    process::ExitCode,
};

const HELP: &str = "Usage: pastebin-js [--json] [--allow-cidr CIDR] <script.js|->

Execute a JavaScript ES module that exports a default function returning a string.
Use - to read source from stdin. HTTPS ESM imports and fetch are supported.

  --allow-cidr CIDR  Allow an otherwise blocked IP range (repeatable)
  --json      Write {output, logs, error, duration_ms} as JSON to stdout
  -h, --help  Show this help
  --version   Show the runner version
  --          End option parsing (for filenames beginning with -)

Default output: returned content on stdout (no added newline), logs/errors on stderr.
Exit codes: 0 success, 1 execution failure, 2 usage or input/configuration error.
PASTEBIN_JS_ALLOW_NET optionally restricts allowed public hosts.
The runner inherits the web executor's limits and network restrictions.
";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Runtime diagnostics must never pollute the script's stdout.
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("pastebin-js: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let mut json = false;
    let mut positional = false;
    let mut path: Option<OsString> = None;
    let mut cidrs = Vec::new();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if !positional {
            if arg == "--allow-cidr" {
                let value = args.next().ok_or("--allow-cidr requires a CIDR")?;
                cidrs.push(value.into_string().map_err(|_| "CIDR must be UTF-8")?);
                continue;
            }
            if arg == "--json" {
                json = true;
                continue;
            }
            if arg == "--" {
                positional = true;
                continue;
            }
            if arg == "--help" || arg == "-h" {
                print!("{HELP}");
                return Ok(0);
            }
            if arg == "--version" {
                println!("pastebin-js {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            if arg != "-" && arg.to_string_lossy().starts_with('-') {
                return Err(format!("unknown option {}; use --help", arg.to_string_lossy()).into());
            }
        }
        if path.replace(arg).is_some() {
            return Err("expected one script file or -; use --help".into());
        }
    }
    let path = path.ok_or("expected a script file or -; use --help")?;
    let input: Box<dyn Read> = if path == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(
            std::fs::File::open(&path)
                .map_err(|error| format!("{}: {error}", path.to_string_lossy()))?,
        )
    };
    let mut source = String::new();
    input
        .take(MAX_SOURCE as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > MAX_SOURCE {
        return Err("JavaScript source exceeds 256 KiB".into());
    }
    let result = ScriptExecutor::from_env_with_allowed_cidrs(&cidrs)?
        .execute(source)
        .await;
    if json {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &result)?;
        writeln!(stdout)?;
    } else {
        for line in &result.logs {
            eprintln!("{line}");
        }
        if let Some(error) = &result.error {
            eprintln!("{error}");
        }
        if let Some(output) = &result.output {
            io::stdout().lock().write_all(output.as_bytes())?;
        }
    }
    Ok(if result.error.is_some() { 1 } else { 0 })
}
