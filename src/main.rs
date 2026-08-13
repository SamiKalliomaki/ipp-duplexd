//! ipp-duplexd - a virtual IPP printer (loopback only) that performs manual
//! duplex printing on a real IPP printer.
//!
//! Jobs printed with sides=two-sided-* (the advertised default) are split
//! with qpdf: odd pages are printed first; when they are done the job stops
//! with reason media-needed until you flip the stack and hit GET /flip (or
//! --auto-continue elapses); then the even pages are printed in reverse
//! order. Jobs with sides=one-sided pass through untouched.
//!
//! Point it at any ipp:// endpoint; a local CUPS queue
//! (ipp://127.0.0.1:631/printers/<name>) works well because CUPS then does
//! any driver work. Printer attributes are proxied from the real printer,
//! so the virtual printer advertises the real hardware's capabilities.

use ipp_duplexd::ipp::{self, Attr, Group, Msg, PrinterUri};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// IPP job states
const J_PENDING: i32 = 3;
const J_PROCESSING: i32 = 5;
const J_STOPPED: i32 = 6;
const J_CANCELED: i32 = 7;
const J_ABORTED: i32 = 8;
const J_COMPLETED: i32 = 9;

#[derive(Clone)]
struct Config {
    listen: String,
    printer: PrinterUri,
    name: String,
    blank: String,      // trailing | leading | none
    auto_continue: u64, // seconds; 0 = wait for /flip or the dialog
    rotate_even: u32,   // 0 | 180
    poll: u64,          // seconds between remote job-state polls
    gui: bool,          // open a desktop dialog to confirm the flip
}

struct Job {
    id: i32,
    name: String,
    user: String,
    state: i32,
    message: String,
    // set for Create-Job jobs awaiting their Send-Document
    pending_attrs: Option<Vec<Attr>>,
}

struct FlipGate {
    requested: Mutex<bool>,
    cv: Condvar,
}

struct Ctx {
    cfg: Config,
    port: u16,
    jobs: Mutex<HashMap<i32, Job>>,
    next_id: AtomicI32,
    flip: FlipGate,
    process_lock: Mutex<()>,
    // (fetched-at, Some(attrs) on success / None when unreachable)
    attr_cache: Mutex<Option<(Instant, Option<Vec<Attr>>)>>,
}

fn log(msg: &str) {
    eprintln!("ipp-duplexd: {msg}");
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ipp-duplexd: {e}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if Command::new("qpdf").arg("--version").output().is_err() {
        eprintln!("ipp-duplexd: qpdf not found in PATH; it is required");
        std::process::exit(1);
    }
    let listener = match TcpListener::bind(&cfg.listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ipp-duplexd: cannot bind {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    let (addr, port) = match listener.local_addr() {
        Ok(a) => (a.to_string(), a.port()),
        Err(_) => (cfg.listen.clone(), 631),
    };
    log(&format!(
        "listening on {} as printer '{}', forwarding to {}",
        addr, cfg.name, cfg.printer.uri
    ));
    log(&format!(
        "add a CUPS queue with: lpadmin -p {} -E -v ipp://127.0.0.1:{}/ipp/print -m everywhere",
        cfg.name, port
    ));
    let ctx = Arc::new(Ctx {
        cfg,
        port,
        jobs: Mutex::new(HashMap::new()),
        next_id: AtomicI32::new(1),
        flip: FlipGate {
            requested: Mutex::new(false),
            cv: Condvar::new(),
        },
        process_lock: Mutex::new(()),
        attr_cache: Mutex::new(None),
    });
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let ctx = ctx.clone();
                std::thread::spawn(move || handle_client(ctx, s));
            }
            Err(e) => log(&format!("accept error: {e}")),
        }
    }
}

const USAGE: &str = "\
usage: ipp-duplexd --printer ipp://HOST[:PORT]/PATH [options]

  --printer URI        real printer (a local CUPS queue works well:
                       ipp://127.0.0.1:631/printers/NAME)
  --listen ADDR:PORT   listen address (default 127.0.0.1:6632)
  --name NAME          printer name to advertise (default manual-duplex)
  --blank WHERE        blank-page padding for odd page counts:
                       leading (default) | trailing | none
  --auto-continue N    print the backs N seconds after the odd pass
                       finishes instead of waiting for GET /flip
  --no-gui             do not open the flip-confirmation desktop dialog
                       (zenity/kdialog/xmessage); rely on GET /flip
  --rotate-even 0|180  rotate even pages 180 degrees (default 180; use 0
                       if the backs come out upside down for your flip
                       direction)
  --poll N             seconds between job-state polls (default 2)";

fn parse_args() -> Result<Config, String> {
    let mut args = std::env::args().skip(1);
    let mut listen = "127.0.0.1:6632".to_string();
    let mut printer = None;
    let mut name = "manual-duplex".to_string();
    let mut blank = "leading".to_string();
    let mut auto_continue = 0u64;
    let mut rotate_even = 180u32;
    let mut poll = 2u64;
    let mut gui = true;
    while let Some(a) = args.next() {
        let mut val = |what: &str| args.next().ok_or(format!("{what} needs a value"));
        match a.as_str() {
            "--listen" => listen = val("--listen")?,
            "--printer" => printer = Some(ipp::parse_uri(&val("--printer")?)?),
            "--name" => name = val("--name")?,
            "--blank" => blank = val("--blank")?,
            "--auto-continue" => {
                auto_continue = val("--auto-continue")?
                    .parse()
                    .map_err(|_| "bad --auto-continue")?
            }
            "--rotate-even" => {
                rotate_even = val("--rotate-even")?
                    .parse()
                    .map_err(|_| "bad --rotate-even")?
            }
            "--poll" => {
                poll = val("--poll")?
                    .parse::<u64>()
                    .map_err(|_| "bad --poll")?
                    .max(1)
            }
            "--gui" => gui = true,
            "--no-gui" => gui = false,
            "-h" | "--help" => return Err("".into()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if !matches!(blank.as_str(), "trailing" | "leading" | "none") {
        return Err("--blank must be trailing, leading or none".into());
    }
    if !matches!(rotate_even, 0 | 180) {
        return Err("--rotate-even must be 0 or 180".into());
    }
    if !listen.starts_with("127.")
        && !listen.starts_with("localhost:")
        && !listen.starts_with("[::1]")
    {
        log(&format!(
            "warning: listening on non-loopback address {listen}"
        ));
    }
    Ok(Config {
        listen,
        printer: printer.ok_or("--printer is required")?,
        name,
        blank,
        auto_continue,
        rotate_even,
        poll,
        gui,
    })
}

// ---------------------------------------------------------------- HTTP layer

fn handle_client(ctx: Arc<Ctx>, stream: TcpStream) {
    stream.set_read_timeout(Some(Duration::from_secs(300))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(60))).ok();
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    // Serve sequential requests on one connection (CUPS reuses it for
    // Create-Job + Send-Document).
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut parts = line.split_whitespace();
        let (method, path) = match (parts.next(), parts.next()) {
            (Some(m), Some(p)) => (m.to_string(), p.to_string()),
            _ => return,
        };
        let mut content_length: Option<usize> = None;
        let mut chunked = false;
        let mut expect_continue = false;
        let mut keep_alive = true;
        loop {
            let mut h = String::new();
            match reader.read_line(&mut h) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if h == "\r\n" || h == "\n" {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = v.trim().parse().ok();
            } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            } else if lower.starts_with("expect:") && lower.contains("100-continue") {
                expect_continue = true;
            } else if lower.starts_with("connection:") && lower.contains("close") {
                keep_alive = false;
            }
        }
        if expect_continue {
            let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = writer.flush();
        }
        let body = if method == "POST" {
            match ipp::read_body(&mut reader, content_length, chunked) {
                Ok(b) => b,
                Err(e) => {
                    log(&format!("bad request body: {e}"));
                    return;
                }
            }
        } else {
            Vec::new()
        };

        let (status, ctype, resp): (&str, &str, Vec<u8>) = match (method.as_str(), path.as_str()) {
            ("POST", _) => match ipp::parse(&body) {
                Ok(req) => {
                    let resp = handle_ipp(&ctx, req);
                    ("200 OK", "application/ipp", resp.encode())
                }
                Err(e) => {
                    log(&format!("unparseable ipp request: {e}"));
                    ("400 Bad Request", "text/plain", e.into_bytes())
                }
            },
            ("GET", "/flip") => {
                let n = trigger_flip(&ctx);
                let msg = if n {
                    "ok, printing the backs\n"
                } else {
                    "no job is waiting for a flip\n"
                };
                ("200 OK", "text/plain", msg.as_bytes().to_vec())
            }
            ("GET", _) => ("200 OK", "text/plain", status_page(&ctx).into_bytes()),
            _ => ("405 Method Not Allowed", "text/plain", b"nope\n".to_vec()),
        };
        let conn = if keep_alive { "keep-alive" } else { "close" };
        let _ = write!(
            writer,
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
            resp.len()
        );
        let _ = writer.write_all(&resp);
        let _ = writer.flush();
        if !keep_alive {
            return;
        }
    }
}

fn status_page(ctx: &Ctx) -> String {
    let jobs = ctx.jobs.lock().unwrap();
    let mut ids: Vec<_> = jobs.keys().copied().collect();
    ids.sort();
    let mut out = format!(
        "ipp-duplexd '{}' -> {}\nGET /flip to continue after flipping the stack\n\n",
        ctx.cfg.name, ctx.cfg.printer.uri
    );
    for id in ids {
        let j = &jobs[&id];
        out.push_str(&format!(
            "job {} \"{}\" ({}): {} {}\n",
            j.id,
            j.name,
            j.user,
            state_name(j.state),
            j.message
        ));
    }
    out
}

fn state_name(s: i32) -> &'static str {
    match s {
        J_PENDING => "pending",
        4 => "pending-held",
        J_PROCESSING => "processing",
        J_STOPPED => "stopped",
        J_CANCELED => "canceled",
        J_ABORTED => "aborted",
        J_COMPLETED => "completed",
        _ => "unknown",
    }
}

fn trigger_flip(ctx: &Ctx) -> bool {
    let waiting = ctx
        .jobs
        .lock()
        .unwrap()
        .values()
        .any(|j| j.state == J_STOPPED);
    let mut req = ctx.flip.requested.lock().unwrap();
    *req = true;
    ctx.flip.cv.notify_all();
    waiting
}

// ----------------------------------------------------------------- IPP layer

fn handle_ipp(ctx: &Arc<Ctx>, req: Msg) -> Msg {
    match req.op_status {
        ipp::OP_GET_PRINTER_ATTRS => op_get_printer_attrs(ctx, &req),
        ipp::OP_VALIDATE_JOB => Msg::response(ipp::OK, req.request_id),
        ipp::OP_PRINT_JOB => op_print_job(ctx, req),
        ipp::OP_CREATE_JOB => op_create_job(ctx, &req),
        ipp::OP_SEND_DOCUMENT => op_send_document(ctx, req),
        ipp::OP_GET_JOBS => op_get_jobs(ctx, &req),
        ipp::OP_GET_JOB_ATTRS => op_get_job_attrs(ctx, &req),
        ipp::OP_CANCEL_JOB => op_cancel_job(ctx, &req),
        other => {
            log(&format!("unsupported operation 0x{other:04x}"));
            Msg::response(ipp::ERR_OP_NOT_SUPPORTED, req.request_id)
        }
    }
}

fn req_user(req: &Msg) -> String {
    req.find_in(ipp::G_OPERATION, "requesting-user-name")
        .and_then(|a| a.first_str())
        .unwrap_or("anonymous")
        .to_string()
}

fn req_job_name(req: &Msg) -> String {
    req.find_in(ipp::G_OPERATION, "job-name")
        .or_else(|| req.find_in(ipp::G_JOB, "job-name"))
        .and_then(|a| a.first_str())
        .unwrap_or("untitled")
        .to_string()
}

fn job_attrs_of(req: &Msg) -> Vec<Attr> {
    req.groups
        .iter()
        .filter(|g| g.tag == ipp::G_JOB)
        .flat_map(|g| g.attrs.iter().cloned())
        .collect()
}

fn job_group(ctx: &Ctx, id: i32, state: i32, message: &str) -> Group {
    Group {
        tag: ipp::G_JOB,
        attrs: vec![
            Attr::int(ipp::T_INTEGER, "job-id", id),
            Attr::str(
                ipp::T_URI,
                "job-uri",
                &format!("ipp://127.0.0.1:{}/jobs/{id}", ctx.port),
            ),
            Attr::int(ipp::T_ENUM, "job-state", state),
            Attr::str(ipp::T_KEYWORD, "job-state-reasons", job_reason(state)),
            Attr::str(ipp::T_TEXT, "job-state-message", message),
        ],
    }
}

fn job_reason(state: i32) -> &'static str {
    match state {
        J_STOPPED => "media-needed",
        J_COMPLETED => "job-completed-successfully",
        J_CANCELED => "job-canceled-by-user",
        J_ABORTED => "aborted-by-system",
        _ => "none",
    }
}

fn op_print_job(ctx: &Arc<Ctx>, req: Msg) -> Msg {
    if req.data.is_empty() {
        return Msg::response(ipp::ERR_BAD_REQUEST, req.request_id);
    }
    let id = new_job(ctx, req_job_name(&req), req_user(&req), None);
    let attrs = job_attrs_of(&req);
    spawn_worker(ctx.clone(), id, req.data.clone(), attrs);
    let mut resp = Msg::response(ipp::OK, req.request_id);
    resp.groups.push(job_group(ctx, id, J_PENDING, ""));
    resp
}

fn op_create_job(ctx: &Arc<Ctx>, req: &Msg) -> Msg {
    let id = new_job(
        ctx,
        req_job_name(req),
        req_user(req),
        Some(job_attrs_of(req)),
    );
    let mut resp = Msg::response(ipp::OK, req.request_id);
    resp.groups.push(job_group(ctx, id, J_PENDING, ""));
    resp
}

fn op_send_document(ctx: &Arc<Ctx>, req: Msg) -> Msg {
    let id = match req
        .find_in(ipp::G_OPERATION, "job-id")
        .and_then(|a| a.first_int())
    {
        Some(i) => i,
        None => return Msg::response(ipp::ERR_BAD_REQUEST, req.request_id),
    };
    let last = req
        .find_in(ipp::G_OPERATION, "last-document")
        .and_then(|a| a.values.first().map(|(_, v)| v == &[1u8]))
        .unwrap_or(true);
    let attrs = {
        let mut jobs = ctx.jobs.lock().unwrap();
        let job = match jobs.get_mut(&id) {
            Some(j) => j,
            None => return Msg::response(ipp::ERR_NOT_FOUND, req.request_id),
        };
        match job.pending_attrs.take() {
            Some(a) => a,
            None => return Msg::response(ipp::ERR_NOT_POSSIBLE, req.request_id),
        }
    };
    if !last {
        // multi-document jobs are not supported
        set_job_state(ctx, id, J_ABORTED, "multi-document jobs not supported");
        return Msg::response(ipp::ERR_OP_NOT_SUPPORTED, req.request_id);
    }
    if req.data.is_empty() {
        set_job_state(ctx, id, J_ABORTED, "empty document");
        return Msg::response(ipp::ERR_BAD_REQUEST, req.request_id);
    }
    spawn_worker(ctx.clone(), id, req.data.clone(), attrs);
    let mut resp = Msg::response(ipp::OK, req.request_id);
    resp.groups.push(job_group(ctx, id, J_PENDING, ""));
    resp
}

fn op_get_jobs(ctx: &Ctx, req: &Msg) -> Msg {
    let which = req
        .find_in(ipp::G_OPERATION, "which-jobs")
        .and_then(|a| a.first_str())
        .unwrap_or("not-completed")
        .to_string();
    let mut resp = Msg::response(ipp::OK, req.request_id);
    let jobs = ctx.jobs.lock().unwrap();
    let mut ids: Vec<_> = jobs.keys().copied().collect();
    ids.sort();
    for id in ids {
        let j = &jobs[&id];
        let done = j.state >= J_CANCELED;
        if (which == "completed") != done {
            continue;
        }
        let mut g = job_group_from(ctx, j);
        g.attrs.push(Attr::str(ipp::T_NAME, "job-name", &j.name));
        g.attrs
            .push(Attr::str(ipp::T_NAME, "job-originating-user-name", &j.user));
        resp.groups.push(g);
    }
    resp
}

fn job_group_from(ctx: &Ctx, j: &Job) -> Group {
    job_group(ctx, j.id, j.state, &j.message)
}

fn op_get_job_attrs(ctx: &Ctx, req: &Msg) -> Msg {
    let id = req
        .find_in(ipp::G_OPERATION, "job-id")
        .and_then(|a| a.first_int());
    let jobs = ctx.jobs.lock().unwrap();
    match id.and_then(|i| jobs.get(&i)) {
        Some(j) => {
            let mut resp = Msg::response(ipp::OK, req.request_id);
            let mut g = job_group_from(ctx, j);
            g.attrs.push(Attr::str(ipp::T_NAME, "job-name", &j.name));
            g.attrs
                .push(Attr::str(ipp::T_NAME, "job-originating-user-name", &j.user));
            resp.groups.push(g);
            resp
        }
        None => Msg::response(ipp::ERR_NOT_FOUND, req.request_id),
    }
}

fn op_cancel_job(ctx: &Ctx, req: &Msg) -> Msg {
    let id = match req
        .find_in(ipp::G_OPERATION, "job-id")
        .and_then(|a| a.first_int())
    {
        Some(i) => i,
        None => return Msg::response(ipp::ERR_BAD_REQUEST, req.request_id),
    };
    let mut jobs = ctx.jobs.lock().unwrap();
    match jobs.get_mut(&id) {
        Some(j) if j.state < J_CANCELED => {
            j.state = J_CANCELED;
            j.message = "canceled".into();
            drop(jobs);
            // wake a worker that may be waiting for the flip
            ctx.flip.cv.notify_all();
            Msg::response(ipp::OK, req.request_id)
        }
        Some(_) => Msg::response(ipp::ERR_NOT_POSSIBLE, req.request_id),
        None => Msg::response(ipp::ERR_NOT_FOUND, req.request_id),
    }
}

// ------------------------------------------------- printer attributes proxy

fn op_get_printer_attrs(ctx: &Ctx, req: &Msg) -> Msg {
    let proxied = proxied_printer_attrs(ctx);
    let online = proxied.is_some();
    let mut attrs = proxied.unwrap_or_else(fallback_printer_attrs);
    apply_overrides(ctx, &mut attrs, online);
    let mut resp = Msg::response(ipp::OK, req.request_id);
    resp.groups.push(Group {
        tag: ipp::G_PRINTER,
        attrs,
    });
    resp
}

fn proxied_printer_attrs(ctx: &Ctx) -> Option<Vec<Attr>> {
    {
        // successes are cached for 30 s, failures for 5 s (so an
        // unreachable printer does not stall every attribute query)
        let cache = ctx.attr_cache.lock().unwrap();
        if let Some((t, result)) = cache.as_ref() {
            let ttl = if result.is_some() { 30 } else { 5 };
            if t.elapsed() < Duration::from_secs(ttl) {
                return result.clone();
            }
        }
    }
    let mut req = Msg::new(ipp::OP_GET_PRINTER_ATTRS, 1);
    req.groups.push(Group {
        tag: ipp::G_OPERATION,
        attrs: vec![
            Attr::str(ipp::T_CHARSET, "attributes-charset", "utf-8"),
            Attr::str(ipp::T_LANGUAGE, "attributes-natural-language", "en"),
            Attr::str(ipp::T_URI, "printer-uri", &ctx.cfg.printer.uri),
            Attr::str(ipp::T_KEYWORD, "requested-attributes", "all"),
        ],
    });
    match ipp::request(&ctx.cfg.printer, &req) {
        Ok(resp) => {
            let attrs: Vec<Attr> = resp
                .groups
                .iter()
                .filter(|g| g.tag == ipp::G_PRINTER)
                .flat_map(|g| g.attrs.iter().cloned())
                .collect();
            let result = if attrs.is_empty() { None } else { Some(attrs) };
            *ctx.attr_cache.lock().unwrap() = Some((Instant::now(), result.clone()));
            result
        }
        Err(e) => {
            log(&format!(
                "cannot fetch printer attributes from real printer: {e}"
            ));
            *ctx.attr_cache.lock().unwrap() = Some((Instant::now(), None));
            None
        }
    }
}

fn fallback_printer_attrs() -> Vec<Attr> {
    vec![
        Attr::str(ipp::T_CHARSET, "charset-configured", "utf-8"),
        Attr::str(ipp::T_CHARSET, "charset-supported", "utf-8"),
        Attr::str(ipp::T_LANGUAGE, "natural-language-configured", "en"),
        Attr::str(
            ipp::T_LANGUAGE,
            "generated-natural-language-supported",
            "en",
        ),
        Attr::strs(ipp::T_KEYWORD, "ipp-versions-supported", &["1.1", "2.0"]),
        Attr::strs(ipp::T_KEYWORD, "pdl-override-supported", &["attempted"]),
        Attr::strs(
            ipp::T_KEYWORD,
            "media-supported",
            &["iso_a4_210x297mm", "na_letter_8.5x11in"],
        ),
        Attr::str(ipp::T_KEYWORD, "media-default", "iso_a4_210x297mm"),
        Attr::str(
            ipp::T_TEXT,
            "printer-make-and-model",
            "ipp-duplexd virtual printer",
        ),
    ]
}

fn apply_overrides(ctx: &Ctx, attrs: &mut Vec<Attr>, online: bool) {
    // capture the real printer's status before stripping, so the virtual
    // printer mirrors it (online/idle/processing/stopped, supply alerts...)
    let real_state = attrs
        .iter()
        .find(|a| a.name == "printer-state")
        .and_then(|a| a.first_int());
    let real_reasons = attrs
        .iter()
        .find(|a| a.name == "printer-state-reasons")
        .cloned();
    let real_message = attrs
        .iter()
        .find(|a| a.name == "printer-state-message")
        .cloned();
    let real_accepting = attrs
        .iter()
        .find(|a| a.name == "printer-is-accepting-jobs")
        .and_then(|a| a.values.first().map(|(_, v)| v.as_slice() != [0u8]));
    let overridden = [
        "printer-uri-supported",
        "uri-security-supported",
        "uri-authentication-supported",
        "printer-name",
        "printer-uuid",
        "printer-state",
        "printer-state-reasons",
        "printer-state-message",
        "printer-is-accepting-jobs",
        "sides-supported",
        "sides-default",
        "operations-supported",
        "document-format-supported",
        "document-format-default",
        "printer-info",
        "printer-location",
        "printer-more-info",
        "queued-job-count",
        "printer-icons",
        "printer-device-id",
        "printer-supply-info-uri",
        "device-uri",
        "printer-alert",
        "printer-alert-description",
    ];
    attrs.retain(|a| !overridden.contains(&a.name.as_str()));

    let (stopped, processing, queued) = {
        let jobs = ctx.jobs.lock().unwrap();
        let active: Vec<_> = jobs.values().filter(|j| j.state < J_CANCELED).collect();
        (
            active.iter().any(|j| j.state == J_STOPPED),
            active.iter().any(|j| j.state == J_PROCESSING),
            active.len() as i32,
        )
    };
    // our own duplex flow wins; otherwise mirror the real printer's state
    let (state, reasons_attr, message_attr) = if stopped {
        (
            5,
            Attr::str(ipp::T_KEYWORD, "printer-state-reasons", "media-needed"),
            Attr::str(
                ipp::T_TEXT,
                "printer-state-message",
                "odd pages printed - flip the stack and confirm",
            ),
        )
    } else if !online {
        (
            5,
            Attr::str(ipp::T_KEYWORD, "printer-state-reasons", "offline-report"),
            Attr::str(
                ipp::T_TEXT,
                "printer-state-message",
                "real printer is unreachable",
            ),
        )
    } else {
        let mirrored = real_state.filter(|s| (3..=5).contains(s)).unwrap_or(3);
        let state = if processing { 4 } else { mirrored };
        (
            state,
            real_reasons
                .unwrap_or_else(|| Attr::str(ipp::T_KEYWORD, "printer-state-reasons", "none")),
            real_message.unwrap_or_else(|| Attr::str(ipp::T_TEXT, "printer-state-message", "")),
        )
    };
    let uri = format!("ipp://127.0.0.1:{}/ipp/print", ctx.port);
    let make = attrs
        .iter()
        .find(|a| a.name == "printer-make-and-model")
        .and_then(|a| a.first_str())
        .unwrap_or("printer")
        .to_string();
    attrs.retain(|a| a.name != "printer-make-and-model");
    attrs.extend([
        Attr::str(ipp::T_URI, "printer-uri-supported", &uri),
        Attr::str(ipp::T_KEYWORD, "uri-security-supported", "none"),
        Attr::str(ipp::T_KEYWORD, "uri-authentication-supported", "none"),
        Attr::str(ipp::T_NAME, "printer-name", &ctx.cfg.name),
        Attr::str(ipp::T_URI, "printer-uuid", &printer_uuid(&ctx.cfg.name)),
        Attr::int(ipp::T_ENUM, "printer-state", state),
        reasons_attr,
        message_attr,
        // keep accepting jobs even when the real printer is offline: we
        // are a spooler, and rejecting would fail the CUPS job outright
        Attr::boolean(
            "printer-is-accepting-jobs",
            real_accepting.unwrap_or(true) || !online,
        ),
        Attr::int(ipp::T_INTEGER, "queued-job-count", queued),
        Attr::strs(
            ipp::T_KEYWORD,
            "sides-supported",
            &["one-sided", "two-sided-long-edge", "two-sided-short-edge"],
        ),
        Attr::str(ipp::T_KEYWORD, "sides-default", "two-sided-long-edge"),
        Attr::ints(
            ipp::T_ENUM,
            "operations-supported",
            &[
                ipp::OP_PRINT_JOB as i32,
                ipp::OP_VALIDATE_JOB as i32,
                ipp::OP_CREATE_JOB as i32,
                ipp::OP_SEND_DOCUMENT as i32,
                ipp::OP_CANCEL_JOB as i32,
                ipp::OP_GET_JOB_ATTRS as i32,
                ipp::OP_GET_JOBS as i32,
                ipp::OP_GET_PRINTER_ATTRS as i32,
            ],
        ),
        Attr::strs(
            ipp::T_MIME,
            "document-format-supported",
            &["application/pdf", "application/octet-stream"],
        ),
        Attr::str(ipp::T_MIME, "document-format-default", "application/pdf"),
        Attr::str(
            ipp::T_TEXT,
            "printer-make-and-model",
            &format!("{make} + manual duplex"),
        ),
        Attr::str(
            ipp::T_TEXT,
            "printer-info",
            &format!("Manual duplex on {make}"),
        ),
        Attr::str(ipp::T_TEXT, "printer-location", "localhost"),
    ]);
}

/// Deterministic urn:uuid derived from the printer name (FNV-1a based).
fn printer_uuid(name: &str) -> String {
    let mut h1: u64 = 0xcbf29ce484222325;
    let mut h2: u64 = 0x811c9dc5811c9dc5;
    for b in name.bytes() {
        h1 = (h1 ^ b as u64).wrapping_mul(0x100000001b3);
        h2 = (h2 ^ b as u64).wrapping_mul(0x1000193);
    }
    let b = [h1.to_be_bytes(), h2.to_be_bytes()].concat();
    format!(
        "urn:uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-8{:01x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6] & 0x0f, b[7], b[8] & 0x0f, b[9],
        b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

// ------------------------------------------------------------------- worker

fn new_job(ctx: &Ctx, name: String, user: String, pending_attrs: Option<Vec<Attr>>) -> i32 {
    let id = ctx.next_id.fetch_add(1, Ordering::SeqCst);
    ctx.jobs.lock().unwrap().insert(
        id,
        Job {
            id,
            name,
            user,
            state: J_PENDING,
            message: String::new(),
            pending_attrs,
        },
    );
    id
}

fn set_job_state(ctx: &Ctx, id: i32, state: i32, message: &str) {
    if let Some(j) = ctx.jobs.lock().unwrap().get_mut(&id) {
        // never resurrect a canceled job
        if j.state == J_CANCELED && state != J_CANCELED {
            return;
        }
        j.state = state;
        j.message = message.to_string();
    }
}

fn job_canceled(ctx: &Ctx, id: i32) -> bool {
    ctx.jobs
        .lock()
        .unwrap()
        .get(&id)
        .map(|j| j.state == J_CANCELED)
        .unwrap_or(true)
}

fn spawn_worker(ctx: Arc<Ctx>, id: i32, data: Vec<u8>, attrs: Vec<Attr>) {
    std::thread::spawn(move || {
        let _serial = ctx.process_lock.lock().unwrap();
        if job_canceled(&ctx, id) {
            return;
        }
        match process_job(&ctx, id, data, attrs) {
            Ok(()) => {}
            Err(e) => {
                log(&format!("job {id} failed: {e}"));
                set_job_state(&ctx, id, J_ABORTED, &e);
            }
        }
    });
}

fn qpdf(args: &[&str]) -> Result<(), String> {
    let out = Command::new("qpdf")
        .arg("--warning-exit-0")
        .args(args)
        .output()
        .map_err(|e| format!("qpdf spawn: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "qpdf {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn qpdf_npages(file: &str) -> Result<u32, String> {
    let out = Command::new("qpdf")
        .args(["--warning-exit-0", "--show-npages", file])
        .output()
        .map_err(|e| format!("qpdf spawn: {e}"))?;
    if !out.status.success() {
        return Err("qpdf could not read the document (PDF input required)".into());
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| "bad page count".into())
}

const BLANK_PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> >>\nendobj\nxref\n0 4\n0000000000 65535 f \ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n0\n%%EOF\n";

fn attr_first_str<'a>(attrs: &'a [Attr], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| a.name == name)
        .and_then(|a| a.first_str())
}

/// Kills a still-running flip dialog when dropped.
struct DialogGuard(Option<Arc<Mutex<Option<std::process::Child>>>>);
impl Drop for DialogGuard {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            if let Some(mut child) = h.lock().unwrap().take() {
                let _ = child.kill();
            }
        }
    }
}

/// Open a desktop "flip the stack" dialog with the first available tool.
/// OK triggers the flip; any other outcome (Cancel, closed window, no
/// display) just logs, so a broken GUI can never cancel a print job —
/// GET /flip keeps working in parallel.
fn spawn_flip_dialog(ctx: &Arc<Ctx>, job: &str) -> Option<Arc<Mutex<Option<std::process::Child>>>> {
    let text = format!(
        "{job}: odd pages printed.\nFlip the stack and reload it, then click OK to print the backs."
    );
    let candidates: [(&str, Vec<&str>); 3] = [
        (
            "zenity",
            vec![
                "--question",
                "--title",
                "ipp-duplexd",
                "--text",
                &text,
                "--ok-label",
                "OK, print backs",
                "--cancel-label",
                "Not yet",
            ],
        ),
        ("kdialog", vec!["--title", "ipp-duplexd", "--yesno", &text]),
        (
            "xmessage",
            vec![
                "-center",
                "-buttons",
                "OK, print backs:101,Not yet:102",
                &text,
            ],
        ),
    ];
    let mut child = None;
    for (bin, args) in &candidates {
        match Command::new(bin).args(args).spawn() {
            Ok(c) => {
                child = Some((*bin, c));
                break;
            }
            Err(_) => continue,
        }
    }
    let Some((bin, child)) = child else {
        log("no dialog tool found (tried zenity, kdialog, xmessage); waiting for GET /flip");
        return None;
    };
    log(&format!("flip dialog opened via {bin}"));
    let handle = Arc::new(Mutex::new(Some(child)));
    let h = handle.clone();
    let ctx = ctx.clone();
    let expect_ok = move |status: std::process::ExitStatus| {
        if bin == "xmessage" {
            status.code() == Some(101)
        } else {
            status.success()
        }
    };
    std::thread::spawn(move || loop {
        {
            let mut g = h.lock().unwrap();
            let Some(c) = g.as_mut() else { return }; // killed by the guard
            match c.try_wait() {
                Ok(Some(status)) => {
                    *g = None;
                    if expect_ok(status) {
                        trigger_flip(&ctx);
                    } else {
                        log("flip dialog dismissed; still waiting for GET /flip");
                    }
                    return;
                }
                Ok(None) => {}
                Err(_) => {
                    *g = None;
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    });
    Some(handle)
}

fn process_job(ctx: &Arc<Ctx>, id: i32, data: Vec<u8>, attrs: Vec<Attr>) -> Result<(), String> {
    set_job_state(ctx, id, J_PROCESSING, "preparing");
    // pid in the name: two ipp-duplexd instances both have a job 1
    let dir = std::env::temp_dir().join(format!("ipp-duplexd-{}-job-{id}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _cleanup = Cleanup(dir.clone());
    let p = |n: &str| dir.join(n).to_string_lossy().into_owned();

    std::fs::write(p("in.pdf"), &data).map_err(|e| e.to_string())?;
    let mut doc = p("in.pdf");

    // page-ranges: apply ourselves (rangeOfInteger values, possibly several)
    if let Some(a) = attrs.iter().find(|a| a.name == "page-ranges") {
        let mut ranges = vec![];
        for (_, v) in &a.values {
            if v.len() == 8 {
                let lo = i32::from_be_bytes(v[0..4].try_into().unwrap());
                let hi = i32::from_be_bytes(v[4..8].try_into().unwrap());
                ranges.push(format!("{lo}-{hi}"));
            }
        }
        if !ranges.is_empty() {
            qpdf(&[
                &doc,
                "--pages",
                ".",
                &ranges.join(","),
                "--",
                &p("ranged.pdf"),
            ])?;
            doc = p("ranged.pdf");
        }
    }

    // copies: replicate so one flip covers all copies, collated
    let copies = attrs
        .iter()
        .find(|a| a.name == "copies")
        .and_then(|a| a.first_int())
        .unwrap_or(1)
        .clamp(1, 999);
    if copies > 1 {
        let mut args: Vec<String> = vec!["--empty".into(), "--pages".into()];
        for _ in 0..copies {
            args.push(doc.clone());
            args.push("1-z".into());
        }
        args.push("--".into());
        args.push(p("copies.pdf"));
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        qpdf(&argrefs)?;
        doc = p("copies.pdf");
    }

    let sides = attr_first_str(&attrs, "sides").unwrap_or("two-sided-long-edge");
    let job_name = ctx
        .jobs
        .lock()
        .unwrap()
        .get(&id)
        .map(|j| j.name.clone())
        .unwrap_or_default();
    let user = ctx
        .jobs
        .lock()
        .unwrap()
        .get(&id)
        .map(|j| j.user.clone())
        .unwrap_or_default();

    if sides == "one-sided" {
        set_job_state(ctx, id, J_PROCESSING, "printing (pass-through)");
        let rid = forward(ctx, &doc, &job_name, &user, &attrs)?;
        wait_remote(ctx, id, rid)?;
        set_job_state(ctx, id, J_COMPLETED, "done");
        return Ok(());
    }

    let npages = qpdf_npages(&doc)?;
    log(&format!(
        "job {id} '{job_name}': {npages} pages, manual duplex"
    ));

    // pass 1: odd pages
    qpdf(&[&doc, "--pages", ".", "1-z:odd", "--", &p("odd.pdf")])?;
    set_job_state(ctx, id, J_PROCESSING, "printing odd pages");
    let rid = forward(
        ctx,
        &p("odd.pdf"),
        &format!("{job_name} [odd pages]"),
        &user,
        &attrs,
    )?;
    wait_remote(ctx, id, rid)?;

    if npages <= 1 {
        set_job_state(ctx, id, J_COMPLETED, "done (single page)");
        return Ok(());
    }

    // pass 2: even pages reversed. For an even page count the odd positions
    // of the reversed range are the even pages descending; for an odd count
    // the even positions.
    let sel = if npages % 2 == 0 {
        "z-1:odd"
    } else {
        "z-1:even"
    };
    qpdf(&[&doc, "--pages", ".", sel, "--", &p("even.pdf")])?;
    let mut even = p("even.pdf");
    if npages % 2 == 1 && ctx.cfg.blank != "none" {
        std::fs::write(p("blank.pdf"), BLANK_PDF).map_err(|e| e.to_string())?;
        let (a, ar, b, br) = if ctx.cfg.blank == "trailing" {
            (even.clone(), "1-z", p("blank.pdf"), "1")
        } else {
            (p("blank.pdf"), "1", even.clone(), "1-z")
        };
        qpdf(&[
            "--empty",
            "--pages",
            &a,
            ar,
            &b,
            br,
            "--",
            &p("even-padded.pdf"),
        ])?;
        even = p("even-padded.pdf");
    }
    if ctx.cfg.rotate_even == 180 {
        qpdf(&[&even, "--rotate=+180:1-z", "--", &p("even-rotated.pdf")])?;
        even = p("even-rotated.pdf");
    }

    // wait for the flip
    *ctx.flip.requested.lock().unwrap() = false;
    let auto = ctx.cfg.auto_continue;
    let deadline = (auto > 0).then(|| Instant::now() + Duration::from_secs(auto));
    let hint = if auto > 0 {
        format!("backs print in {auto}s")
    } else if ctx.cfg.gui {
        "click OK in the dialog".to_string()
    } else {
        format!("then: curl http://127.0.0.1:{}/flip", ctx.port)
    };
    set_job_state(ctx, id, J_STOPPED, &format!("flip the stack; {hint}"));
    log(&format!("job {id}: odd pages done. Flip the stack; {hint}"));
    // the guard kills a still-open dialog on every exit path
    let _dialog = if ctx.cfg.gui && auto == 0 {
        DialogGuard(spawn_flip_dialog(ctx, &job_name))
    } else {
        DialogGuard(None)
    };
    loop {
        if job_canceled(ctx, id) {
            return Ok(());
        }
        let mut req = ctx.flip.requested.lock().unwrap();
        if *req || deadline.is_some_and(|d| Instant::now() >= d) {
            *req = false;
            break;
        }
        let _ = ctx
            .flip
            .cv
            .wait_timeout(req, Duration::from_secs(1))
            .unwrap();
    }

    set_job_state(ctx, id, J_PROCESSING, "printing even pages");
    let rid = forward(
        ctx,
        &even,
        &format!("{job_name} [even pages]"),
        &user,
        &attrs,
    )?;
    wait_remote(ctx, id, rid)?;
    set_job_state(ctx, id, J_COMPLETED, "done");
    log(&format!("job {id} completed"));
    Ok(())
}

struct Cleanup(std::path::PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Job attributes that must NOT be forwarded: either we applied them
/// ourselves or they would change the pagination underneath our split.
const NO_FORWARD: &[&str] = &[
    "sides",
    "copies",
    "page-ranges",
    "number-up",
    "number-up-layout",
    "page-border",
    "multiple-document-handling",
    "job-hold-until",
];

fn forward(ctx: &Ctx, file: &str, name: &str, user: &str, attrs: &[Attr]) -> Result<i32, String> {
    let data = std::fs::read(file).map_err(|e| e.to_string())?;
    let mut msg = Msg::new(ipp::OP_PRINT_JOB, 1);
    msg.groups.push(Group {
        tag: ipp::G_OPERATION,
        attrs: vec![
            Attr::str(ipp::T_CHARSET, "attributes-charset", "utf-8"),
            Attr::str(ipp::T_LANGUAGE, "attributes-natural-language", "en"),
            Attr::str(ipp::T_URI, "printer-uri", &ctx.cfg.printer.uri),
            Attr::str(ipp::T_NAME, "requesting-user-name", user),
            Attr::str(ipp::T_NAME, "job-name", name),
            Attr::str(ipp::T_MIME, "document-format", "application/pdf"),
        ],
    });
    let mut fwd: Vec<Attr> = attrs
        .iter()
        .filter(|a| !NO_FORWARD.contains(&a.name.as_str()))
        .cloned()
        .collect();
    fwd.push(Attr::str(ipp::T_KEYWORD, "sides", "one-sided"));
    msg.groups.push(Group {
        tag: ipp::G_JOB,
        attrs: fwd,
    });
    msg.data = data;
    let resp = ipp::request(&ctx.cfg.printer, &msg)?;
    if resp.op_status > 0x00ff {
        return Err(format!(
            "real printer rejected the job (ipp status 0x{:04x})",
            resp.op_status
        ));
    }
    resp.find("job-id")
        .and_then(|a| a.first_int())
        .ok_or_else(|| "real printer returned no job-id".into())
}

fn wait_remote(ctx: &Ctx, id: i32, remote_id: i32) -> Result<(), String> {
    let mut errors = 0;
    loop {
        if job_canceled(ctx, id) {
            // best effort: cancel the remote job too
            let mut c = Msg::new(ipp::OP_CANCEL_JOB, 1);
            c.groups.push(Group {
                tag: ipp::G_OPERATION,
                attrs: vec![
                    Attr::str(ipp::T_CHARSET, "attributes-charset", "utf-8"),
                    Attr::str(ipp::T_LANGUAGE, "attributes-natural-language", "en"),
                    Attr::str(ipp::T_URI, "printer-uri", &ctx.cfg.printer.uri),
                    Attr::int(ipp::T_INTEGER, "job-id", remote_id),
                ],
            });
            let _ = ipp::request(&ctx.cfg.printer, &c);
            return Ok(());
        }
        let mut q = Msg::new(ipp::OP_GET_JOB_ATTRS, 1);
        q.groups.push(Group {
            tag: ipp::G_OPERATION,
            attrs: vec![
                Attr::str(ipp::T_CHARSET, "attributes-charset", "utf-8"),
                Attr::str(ipp::T_LANGUAGE, "attributes-natural-language", "en"),
                Attr::str(ipp::T_URI, "printer-uri", &ctx.cfg.printer.uri),
                Attr::int(ipp::T_INTEGER, "job-id", remote_id),
                Attr::str(ipp::T_KEYWORD, "requested-attributes", "job-state"),
            ],
        });
        match ipp::request(&ctx.cfg.printer, &q) {
            Ok(resp) => {
                errors = 0;
                match resp.find("job-state").and_then(|a| a.first_int()) {
                    Some(s) if s == J_COMPLETED => return Ok(()),
                    Some(s) if s == J_CANCELED || s == J_ABORTED => {
                        return Err(format!(
                            "remote job {remote_id} ended in state {}",
                            state_name(s)
                        ));
                    }
                    _ => {}
                }
            }
            Err(e) => {
                errors += 1;
                if errors >= 5 {
                    return Err(format!("lost contact with real printer: {e}"));
                }
            }
        }
        std::thread::sleep(Duration::from_secs(ctx.cfg.poll));
    }
}
