//! Interactive setup for ipp-duplexd: pick the real printer, write the config,
//! enable the systemd user service, and register the virtual printer as a
//! driverless CUPS queue.
//!
//! Every step can also be skipped or overridden with flags, and --yes runs
//! non-interactively with defaults.

use ipp_duplexd::ipp::{self, Attr, Group, Msg};
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ASSUME_DEFAULTS: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "\
ipp-duplexd-setup — set up the manual duplex virtual printer

Walks through: choosing the real printer, writing ~/.config/ipp-duplexd.conf,
enabling the ipp-duplexd systemd user service, and adding the virtual printer
as a driverless CUPS queue.

Options (all optional; anything not given is asked interactively):
  --printer URI     real printer, e.g. ipp://127.0.0.1:631/printers/laser
  --queue NAME      CUPS queue name for the virtual printer
  --listen ADDR     ipp-duplexd listen address       (default 127.0.0.1:6632)
  --no-service      skip the systemd service step
  --no-queue        skip the CUPS queue step
  --yes             no questions; use defaults for anything not given
  -h, --help        this help
";

fn main() {
    let mut printer_uri: Option<String> = None;
    let mut queue_name: Option<String> = None;
    let mut listen = "127.0.0.1:6632".to_string();
    let mut do_service = true;
    let mut do_queue = true;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--printer" => printer_uri = args.next(),
            "--queue" => queue_name = args.next(),
            "--listen" => listen = args.next().unwrap_or(listen),
            "--no-service" => do_service = false,
            "--no-queue" => do_queue = false,
            "--yes" => ASSUME_DEFAULTS.store(true, Ordering::Relaxed),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("unknown option '{other}'\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    println!("ipp-duplexd setup — manual duplex virtual printer");
    println!("=============================================");

    // 1. prerequisites -----------------------------------------------------
    if !have("qpdf") {
        println!();
        println!("! qpdf is not installed — ipp-duplexd needs it to split pages.");
        println!("  Install it first (e.g. 'sudo pacman -S qpdf' / 'sudo apt install qpdf').");
        if !confirm("Continue anyway?", false) {
            std::process::exit(1);
        }
    }
    let daemon_bin = find_duplexd().unwrap_or_else(|| {
        println!("! could not find the 'ipp-duplexd' binary (not in PATH or next to this tool).");
        println!("  Install the package or run 'cargo build --release' first.");
        std::process::exit(1);
    });

    // 2. real printer ------------------------------------------------------
    println!();
    println!("Step 1: the real printer ipp-duplexd forwards to");
    let printer_uri = printer_uri.unwrap_or_else(choose_printer);
    match ipp::parse_uri(&printer_uri) {
        Err(e) => {
            println!("! '{printer_uri}' is not a usable printer URI: {e}");
            std::process::exit(1);
        }
        Ok(uri) => match probe(&uri) {
            Some(mm) => println!("  found: {mm}"),
            None => {
                println!("! no IPP printer answered at {printer_uri}");
                println!("  (fine if it is just switched off right now)");
                if !confirm("Use it anyway?", true) {
                    std::process::exit(1);
                }
            }
        },
    }

    // 3. options + config file ---------------------------------------------
    println!();
    println!("Step 2: ipp-duplexd configuration (~/.config/ipp-duplexd.conf)");
    let mut daemon_args = format!("--printer {printer_uri}");
    if listen != "127.0.0.1:6632" {
        daemon_args += &format!(" --listen {listen}");
    }
    let mut gui = true;
    if confirm(
        "Change advanced options (flip dialog, rotation, blank page)?",
        false,
    ) {
        if !confirm(
            "Open a desktop dialog when it is time to flip? (else: curl /flip)",
            true,
        ) {
            daemon_args += " --no-gui";
            gui = false;
        }
        if !confirm("Rotate the even (back) pages by 180°?", true) {
            daemon_args += " --rotate-even 0";
        }
        let blank = ask(
            "Blank-page padding for odd page counts (leading/trailing/none)",
            "leading",
        );
        if blank != "leading" {
            daemon_args += &format!(" --blank {blank}");
        }
    }
    write_config(&daemon_args);

    // 4. service ------------------------------------------------------------
    println!();
    println!("Step 3: run ipp-duplexd as a service");
    let mut running = false;
    if do_service && have("systemctl") {
        running = setup_service(&daemon_bin, gui);
    } else if do_service {
        println!("  systemd not found — start ipp-duplexd yourself:");
        println!("    {} {daemon_args}", daemon_bin.display());
    } else {
        println!("  skipped (--no-service)");
    }

    // 5. verify the virtual printer ----------------------------------------
    let port = listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(6632);
    let self_uri = format!("ipp://127.0.0.1:{port}/ipp/print");
    if running {
        print!("  waiting for ipp-duplexd on {listen} ");
        io::stdout().flush().ok();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running_at(port) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            print!(".");
            io::stdout().flush().ok();
        }
        println!();
        match ipp::parse_uri(&self_uri).ok().and_then(|u| probe(&u)) {
            Some(mm) => println!("  virtual printer is up: {mm}"),
            None => println!("! ipp-duplexd is not answering on {listen} — check: journalctl --user -u ipp-duplexd"),
        }
    }

    // 6. CUPS queue ----------------------------------------------------------
    println!();
    println!("Step 4: add the virtual printer to CUPS");
    if do_queue && have("lpadmin") {
        let default = queue_default(&printer_uri);
        let queue = queue_name.unwrap_or_else(|| ask("CUPS queue name", &default));
        add_queue(&queue, &self_uri);
    } else if do_queue {
        println!("  lpadmin (CUPS) not found — on the machine you print from, run:");
        println!("    lpadmin -p manual-duplex -E -v {self_uri} -m everywhere");
    } else {
        println!("  skipped (--no-queue)");
    }

    println!();
    println!("Done. Print to the new queue; after the odd pages, flip the stack");
    println!("and confirm in the dialog (or: curl http://127.0.0.1:{port}/flip).");
    println!("Status page: http://127.0.0.1:{port}/");
}

// ---------------------------------------------------------------- questions

fn ask(prompt: &str, default: &str) -> String {
    if ASSUME_DEFAULTS.load(Ordering::Relaxed) {
        return default.into();
    }
    print!("{prompt} [{default}]: ");
    io::stdout().flush().ok();
    let mut s = String::new();
    if io::stdin().read_line(&mut s).unwrap_or(0) == 0 {
        println!();
        return default.into(); // EOF
    }
    let t = s.trim();
    if t.is_empty() {
        default.into()
    } else {
        t.into()
    }
}

fn confirm(prompt: &str, default: bool) -> bool {
    let d = if default { "Y/n" } else { "y/N" };
    let a = ask(prompt, d);
    match a.to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

// ------------------------------------------------------------------ helpers

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty() || !o.stdout.is_empty())
        .unwrap_or(false)
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = format!("exit status {}", out.status);
        }
        Err(msg)
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn find_duplexd() -> Option<PathBuf> {
    // next to this binary first (cargo target dir, or /usr/bin for packages)
    if let Ok(me) = std::env::current_exe() {
        let sibling = me.with_file_name("ipp-duplexd");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .map(|d| Path::new(d).join("ipp-duplexd"))
        .find(|p| p.is_file())
}

fn running_at(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Get-Printer-Attributes; Some(make-and-model) if an IPP printer answers.
fn probe(uri: &ipp::PrinterUri) -> Option<String> {
    let mut m = Msg::new(ipp::OP_GET_PRINTER_ATTRS, 1);
    m.groups.push(Group {
        tag: ipp::G_OPERATION,
        attrs: vec![
            Attr::str(ipp::T_CHARSET, "attributes-charset", "utf-8"),
            Attr::str(ipp::T_LANGUAGE, "attributes-natural-language", "en"),
            Attr::str(ipp::T_URI, "printer-uri", &uri.uri),
        ],
    });
    let resp = ipp::request(uri, &m).ok()?;
    Some(
        resp.find("printer-make-and-model")
            .and_then(|a| a.first_str())
            .unwrap_or("(unnamed IPP printer)")
            .to_string(),
    )
}

// ------------------------------------------------------------ printer choice

/// All CUPS queues as (name, device-uri), parsed from `lpstat -v`.
fn cups_queues() -> Vec<(String, String)> {
    let Ok(out) = run("lpstat", &["-v"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| {
            // "device for laser: ipp://host/..."
            let rest = l.strip_prefix("device for ")?;
            let (name, uri) = rest.split_once(':')?;
            Some((name.trim().to_string(), uri.trim().to_string()))
        })
        .collect()
}

fn choose_printer() -> String {
    // skip queues on loopback:6632 — that would be ipp-duplexd forwarding to itself
    let queues: Vec<_> = cups_queues()
        .into_iter()
        .filter(|(_, uri)| !uri.contains("127.0.0.1:6632") && !uri.contains("localhost:6632"))
        .collect();
    if queues.is_empty() {
        println!("  no CUPS queues found; enter the printer's IPP endpoint directly.");
        return ask("Real printer URI", "ipp://192.168.1.100:631/ipp/print");
    }
    println!("  going through the local CUPS queue is recommended: CUPS keeps doing");
    println!("  any driver work, and TLS-only printers work too.");
    for (i, (name, uri)) in queues.iter().enumerate() {
        println!("    {}) {name}  ({uri})", i + 1);
    }
    println!("    or enter an ipp:// URI directly");
    loop {
        let a = ask("Real printer", "1");
        if let Ok(n) = a.parse::<usize>() {
            if n >= 1 && n <= queues.len() {
                return format!("ipp://127.0.0.1:631/printers/{}", queues[n - 1].0);
            }
        }
        if a.starts_with("ipp://") || a.starts_with("http://") {
            return a;
        }
        println!("  enter a number 1-{} or an ipp:// URI", queues.len());
        if ASSUME_DEFAULTS.load(Ordering::Relaxed) {
            std::process::exit(1); // avoid looping forever with --yes
        }
    }
}

/// "laser-duplex" when forwarding to a CUPS queue, else "manual-duplex".
fn queue_default(printer_uri: &str) -> String {
    printer_uri
        .rsplit_once("/printers/")
        .map(|(_, q)| format!("{q}-duplex"))
        .unwrap_or_else(|| "manual-duplex".into())
}

// -------------------------------------------------------------- config file

fn write_config(daemon_args: &str) {
    let path = home().join(".config/ipp-duplexd.conf");
    let line = format!("IPP_DUPLEXD_ARGS={daemon_args}\n");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == line {
            println!("  {} already up to date", path.display());
            return;
        }
        println!("  {} exists:", path.display());
        for l in existing.lines().take(5) {
            println!("    | {l}");
        }
        if !confirm("Overwrite it?", true) {
            println!("  keeping the existing config");
            return;
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    match std::fs::write(&path, &line) {
        Ok(()) => println!("  wrote {}: {}", path.display(), line.trim_end()),
        Err(e) => {
            println!("! cannot write {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

// ------------------------------------------------------------------ service

/// Enable + (re)start the user service; returns true if it should be running.
fn setup_service(daemon_bin: &Path, gui: bool) -> bool {
    // package-installed unit, or write one ourselves
    let packaged = Path::new("/usr/lib/systemd/user/ipp-duplexd.service").is_file()
        || Path::new("/lib/systemd/user/ipp-duplexd.service").is_file();
    let user_unit = home().join(".config/systemd/user/ipp-duplexd.service");
    if !packaged && !user_unit.is_file() {
        // With the dialog, bind to the graphical session: it starts once the
        // display variables exist and stops when they go away. Without it,
        // there is nothing session-bound about the daemon, so run it for the
        // whole login (it still serves GET /flip over ssh, with no desktop up).
        let target = if gui {
            "After=graphical-session.target\nPartOf=graphical-session.target\n"
        } else {
            ""
        };
        let wanted_by = if gui {
            "graphical-session.target"
        } else {
            "default.target"
        };
        let unit = format!(
            "[Unit]\nDescription=Manual duplex virtual IPP printer\nAfter=network.target\n\
             {target}\n\
             [Service]\nEnvironmentFile=%h/.config/ipp-duplexd.conf\n\
             ExecStart={} $IPP_DUPLEXD_ARGS\nRestart=on-failure\nRestartSec=5\n\n\
             [Install]\nWantedBy={wanted_by}\n",
            daemon_bin.display()
        );
        std::fs::create_dir_all(user_unit.parent().unwrap()).ok();
        match std::fs::write(&user_unit, unit) {
            Ok(()) => println!("  installed {}", user_unit.display()),
            Err(e) => {
                println!("! cannot write {}: {e}", user_unit.display());
                return false;
            }
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    // The dialog needs the session's display variables in the service
    // environment. A desktop session normally imports them into the user
    // manager itself at login; warn if this one did not, since the dialog will
    // not open until they are there.
    if gui {
        let manager_env = run("systemctl", &["--user", "show-environment"]).unwrap_or_default();
        let missing: Vec<String> = ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .filter(|v| {
                !manager_env
                    .lines()
                    .any(|l| l.split('=').next() == Some(**v))
            })
            .filter_map(|v| {
                let val = std::env::var_os(v)?;
                Some(format!("{v}={}", val.to_string_lossy()))
            })
            .collect();
        if !missing.is_empty() {
            println!("! this desktop session does not export the display variables to systemd");
            println!("  the flip dialog will not open until it can reach your display.");
            println!("  put them in ~/.config/environment.d/ipp-duplexd.conf:");
            for kv in &missing {
                println!("      {kv}");
            }
        }
    }
    match run("systemctl", &["--user", "enable", "--now", "ipp-duplexd"]) {
        Ok(_) => {}
        Err(e) => {
            println!("! systemctl --user enable --now ipp-duplexd failed: {e}");
            println!(
                "  start it manually instead: {} <args from ~/.config/ipp-duplexd.conf>",
                daemon_bin.display()
            );
            return false;
        }
    }
    // restart in case it was already running with the old config
    let _ = run("systemctl", &["--user", "restart", "ipp-duplexd"]);
    println!(
        "  service ipp-duplexd enabled and started (journalctl --user -u ipp-duplexd for logs)"
    );
    true
}

// --------------------------------------------------------------- CUPS queue

fn add_queue(queue: &str, self_uri: &str) {
    // already pointing at us?
    if let Some((_, uri)) = cups_queues().into_iter().find(|(n, _)| n == queue) {
        if uri == self_uri {
            println!("  CUPS queue '{queue}' already points at ipp-duplexd");
            return;
        }
        println!("  CUPS queue '{queue}' exists with a different device ({uri})");
        if !confirm("Repoint it at ipp-duplexd?", false) {
            return;
        }
    }
    match run(
        "lpadmin",
        &["-p", queue, "-E", "-v", self_uri, "-m", "everywhere"],
    ) {
        Ok(_) => {
            println!("  added CUPS queue '{queue}' -> {self_uri}");
            println!("  test it with: lp -d {queue} some.pdf");
        }
        Err(e) => {
            println!("! lpadmin failed: {e}");
            println!("  you may need admin rights; try:");
            println!("    sudo lpadmin -p {queue} -E -v {self_uri} -m everywhere");
        }
    }
}
