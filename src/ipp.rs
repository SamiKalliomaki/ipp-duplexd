//! Minimal IPP (RFC 8010/8011) message encoding, decoding and a small
//! HTTP/1.1 client for talking to the real printer. Attribute values are
//! kept as raw bytes so unknown attributes round-trip byte-faithfully
//! (needed for proxying the real printer's attributes).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

// Delimiter (group) tags
pub const G_OPERATION: u8 = 0x01;
pub const G_JOB: u8 = 0x02;
pub const G_END: u8 = 0x03;
pub const G_PRINTER: u8 = 0x04;

// Value tags
pub const T_INTEGER: u8 = 0x21;
pub const T_BOOLEAN: u8 = 0x22;
pub const T_ENUM: u8 = 0x23;
pub const T_TEXT: u8 = 0x41;
pub const T_NAME: u8 = 0x42;
pub const T_KEYWORD: u8 = 0x44;
pub const T_URI: u8 = 0x45;
pub const T_CHARSET: u8 = 0x47;
pub const T_LANGUAGE: u8 = 0x48;
pub const T_MIME: u8 = 0x49;

// Operations
pub const OP_PRINT_JOB: u16 = 0x0002;
pub const OP_VALIDATE_JOB: u16 = 0x0004;
pub const OP_CREATE_JOB: u16 = 0x0005;
pub const OP_SEND_DOCUMENT: u16 = 0x0006;
pub const OP_CANCEL_JOB: u16 = 0x0008;
pub const OP_GET_JOB_ATTRS: u16 = 0x0009;
pub const OP_GET_JOBS: u16 = 0x000A;
pub const OP_GET_PRINTER_ATTRS: u16 = 0x000B;

// Status codes
pub const OK: u16 = 0x0000;
pub const ERR_BAD_REQUEST: u16 = 0x0400;
pub const ERR_NOT_FOUND: u16 = 0x0406;
pub const ERR_NOT_POSSIBLE: u16 = 0x0409;
pub const ERR_INTERNAL: u16 = 0x0500;
pub const ERR_OP_NOT_SUPPORTED: u16 = 0x0501;

/// One attribute: a name plus one or more (value-tag, raw-value) pairs.
/// Additional values (and collection members) keep their own tags.
#[derive(Clone, Debug)]
pub struct Attr {
    pub name: String,
    pub values: Vec<(u8, Vec<u8>)>,
}

impl Attr {
    pub fn str(tag: u8, name: &str, v: &str) -> Attr {
        Attr {
            name: name.into(),
            values: vec![(tag, v.as_bytes().to_vec())],
        }
    }
    pub fn strs(tag: u8, name: &str, vs: &[&str]) -> Attr {
        Attr {
            name: name.into(),
            values: vs.iter().map(|v| (tag, v.as_bytes().to_vec())).collect(),
        }
    }
    pub fn int(tag: u8, name: &str, v: i32) -> Attr {
        Attr {
            name: name.into(),
            values: vec![(tag, v.to_be_bytes().to_vec())],
        }
    }
    pub fn ints(tag: u8, name: &str, vs: &[i32]) -> Attr {
        Attr {
            name: name.into(),
            values: vs.iter().map(|v| (tag, v.to_be_bytes().to_vec())).collect(),
        }
    }
    pub fn boolean(name: &str, v: bool) -> Attr {
        Attr {
            name: name.into(),
            values: vec![(T_BOOLEAN, vec![v as u8])],
        }
    }
    pub fn first_str(&self) -> Option<&str> {
        self.values
            .first()
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    }
    pub fn first_int(&self) -> Option<i32> {
        self.values
            .first()
            .and_then(|(_, v)| v.as_slice().try_into().ok().map(i32::from_be_bytes))
    }
}

#[derive(Clone, Debug)]
pub struct Group {
    pub tag: u8,
    pub attrs: Vec<Attr>,
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub version: (u8, u8),
    /// operation-id in requests, status-code in responses
    pub op_status: u16,
    pub request_id: u32,
    pub groups: Vec<Group>,
    pub data: Vec<u8>,
}

impl Msg {
    pub fn new(op_status: u16, request_id: u32) -> Msg {
        Msg {
            version: (2, 0),
            op_status,
            request_id,
            groups: vec![],
            data: vec![],
        }
    }

    /// Standard response skeleton with the mandatory operation attributes.
    pub fn response(status: u16, request_id: u32) -> Msg {
        let mut m = Msg::new(status, request_id);
        m.groups.push(Group {
            tag: G_OPERATION,
            attrs: vec![
                Attr::str(T_CHARSET, "attributes-charset", "utf-8"),
                Attr::str(T_LANGUAGE, "attributes-natural-language", "en"),
            ],
        });
        m
    }

    pub fn find(&self, name: &str) -> Option<&Attr> {
        self.groups
            .iter()
            .flat_map(|g| g.attrs.iter())
            .find(|a| a.name == name)
    }

    pub fn find_in(&self, group_tag: u8, name: &str) -> Option<&Attr> {
        self.groups
            .iter()
            .filter(|g| g.tag == group_tag)
            .flat_map(|g| g.attrs.iter())
            .find(|a| a.name == name)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.data.len());
        out.push(self.version.0);
        out.push(self.version.1);
        out.extend_from_slice(&self.op_status.to_be_bytes());
        out.extend_from_slice(&self.request_id.to_be_bytes());
        for g in &self.groups {
            out.push(g.tag);
            for a in &g.attrs {
                for (i, (tag, val)) in a.values.iter().enumerate() {
                    out.push(*tag);
                    if i == 0 {
                        out.extend_from_slice(&(a.name.len() as u16).to_be_bytes());
                        out.extend_from_slice(a.name.as_bytes());
                    } else {
                        out.extend_from_slice(&0u16.to_be_bytes());
                    }
                    out.extend_from_slice(&(val.len() as u16).to_be_bytes());
                    out.extend_from_slice(val);
                }
            }
        }
        out.push(G_END);
        out.extend_from_slice(&self.data);
        out
    }
}

pub fn parse(buf: &[u8]) -> Result<Msg, String> {
    if buf.len() < 9 {
        return Err("ipp message too short".into());
    }
    let mut msg = Msg {
        version: (buf[0], buf[1]),
        op_status: u16::from_be_bytes([buf[2], buf[3]]),
        request_id: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        groups: vec![],
        data: vec![],
    };
    let mut i = 8usize;
    let mut cur: Option<Group> = None;
    loop {
        if i >= buf.len() {
            break; // tolerate missing end-of-attributes tag
        }
        let tag = buf[i];
        i += 1;
        if tag == G_END {
            msg.data = buf[i..].to_vec();
            break;
        }
        if tag <= 0x0f {
            if let Some(g) = cur.take() {
                msg.groups.push(g);
            }
            cur = Some(Group { tag, attrs: vec![] });
            continue;
        }
        // value-tag name-len name value-len value
        let need = |i: usize, n: usize| -> Result<(), String> {
            if i + n > buf.len() {
                Err("truncated ipp attribute".into())
            } else {
                Ok(())
            }
        };
        need(i, 2)?;
        let nlen = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
        i += 2;
        need(i, nlen)?;
        let name = String::from_utf8_lossy(&buf[i..i + nlen]).into_owned();
        i += nlen;
        need(i, 2)?;
        let vlen = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
        i += 2;
        need(i, vlen)?;
        let value = buf[i..i + vlen].to_vec();
        i += vlen;
        let g = cur.as_mut().ok_or("attribute outside of any group")?;
        if nlen == 0 {
            // additional value (or collection member) for the previous attribute
            match g.attrs.last_mut() {
                Some(prev) => prev.values.push((tag, value)),
                None => return Err("additional value with no previous attribute".into()),
            }
        } else {
            g.attrs.push(Attr {
                name,
                values: vec![(tag, value)],
            });
        }
    }
    if let Some(g) = cur.take() {
        msg.groups.push(g);
    }
    Ok(msg)
}

/// ipp://host[:port]/path parsed into connectable pieces.
#[derive(Clone, Debug)]
pub struct PrinterUri {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub uri: String,
}

pub fn parse_uri(uri: &str) -> Result<PrinterUri, String> {
    let rest = uri
        .strip_prefix("ipp://")
        .or_else(|| uri.strip_prefix("http://"))
        .ok_or_else(|| format!("unsupported printer URI '{uri}' (need ipp:// or http://; TLS-only ipps:// is not supported)"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| "bad port")?),
        None => (hostport.to_string(), 631),
    };
    if host.is_empty() {
        return Err("empty host in printer URI".into());
    }
    Ok(PrinterUri {
        host,
        port,
        path: path.to_string(),
        uri: uri.to_string(),
    })
}

/// POST an IPP message (plus optional trailing document data already inside
/// `msg.data`) and return the parsed IPP response.
pub fn request(target: &PrinterUri, msg: &Msg) -> Result<Msg, String> {
    let body = msg.encode();
    let addr = format!("{}:{}", target.host, target.port);
    let resolved = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {addr}"))?;
    let stream = TcpStream::connect_timeout(&resolved, Duration::from_secs(5))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    write!(
        writer,
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nUser-Agent: ipp-duplexd/0.1\r\nConnection: close\r\n\r\n",
        target.path, addr, body.len()
    )
    .map_err(|e| e.to_string())?;
    writer.write_all(&body).map_err(|e| e.to_string())?;
    writer.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let status = line.split_whitespace().nth(1).unwrap_or("");
    // skip interim 100 Continue responses
    if status == "100" {
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).map_err(|e| e.to_string())?;
            if l == "\r\n" || l == "\n" || l.is_empty() {
                break;
            }
        }
        line.clear();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
    }
    let status = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).map_err(|e| e.to_string())?;
        if l == "\r\n" || l == "\n" || l.is_empty() {
            break;
        }
        let lower = l.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    if status != "200" {
        return Err(format!("http status {status} from {}", target.uri));
    }
    let body = read_body(&mut reader, content_length, chunked)?;
    parse(&body)
}

pub fn read_body<R: BufRead>(
    reader: &mut R,
    content_length: Option<usize>,
    chunked: bool,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).map_err(|e| e.to_string())?;
            let size = usize::from_str_radix(l.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| format!("bad chunk size line {l:?}"))?;
            if size == 0 {
                // trailer + final CRLF
                loop {
                    let mut t = String::new();
                    reader.read_line(&mut t).map_err(|e| e.to_string())?;
                    if t == "\r\n" || t == "\n" || t.is_empty() {
                        break;
                    }
                }
                break;
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk).map_err(|e| e.to_string())?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).map_err(|e| e.to_string())?;
        }
    } else if let Some(n) = content_length {
        body = vec![0u8; n];
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    } else {
        reader.read_to_end(&mut body).map_err(|e| e.to_string())?;
    }
    Ok(body)
}
