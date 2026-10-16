//! Minimal IMAP client over implicit TLS, plus the shared [`BufReader`]
//! used by the SMTP client too.
//!
//! Stateless by design: one TCP+TLS connection per bridge request,
//! LOGIN -> command(s) -> LOGOUT. No session is ever kept.

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

#[derive(Debug)]
pub struct MailError(pub String);

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for MailError {}

pub type Result<T> = std::result::Result<T, MailError>;

pub(crate) fn err(msg: impl Into<String>) -> MailError {
    MailError(msg.into())
}

async fn tls_connect(host: &str, port: u16) -> Result<TlsStream<TcpStream>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| err(format!("tcp connect {}:{} failed: {}", host, port, e)))?;
    let sn = ServerName::try_from(host)
        .map_err(|_| err(format!("invalid hostname: {}", host)))?
        .to_owned();
    connector
        .connect(sn, tcp)
        .await
        .map_err(|e| err(format!("tls handshake {}:{} failed: {}", host, port, e)))
}

/// Persistent-buffer byte reader over a TLS stream.
///
/// Unlike a naive per-line reader, leftover bytes after a CRLF are kept,
/// so multi-line SMTP replies (e.g. EHLO's `250-...` block) can never be
/// truncated by an over-eager read. Also handles IMAP `{n}` literals.
pub(crate) struct BufReader {
    stream: TlsStream<TcpStream>,
    buf: Vec<u8>,
}

impl BufReader {
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        Ok(Self {
            stream: tls_connect(host, port).await?,
            buf: Vec::new(),
        })
    }

    async fn fill(&mut self) -> Result<()> {
        let mut tmp = [0u8; 8192];
        let n = self
            .stream
            .read(&mut tmp)
            .await
            .map_err(|e| err(format!("read failed: {}", e)))?;
        if n == 0 {
            return Err(err("connection closed by server"));
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    pub async fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
                self.buf.drain(..pos + 2);
                return Ok(line);
            }
            self.fill().await?;
        }
    }

    async fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        while self.buf.len() < n {
            self.fill().await?;
        }
        let out = self.buf[..n].to_vec();
        self.buf.drain(..n);
        Ok(out)
    }

    /// One IMAP "record": a line plus any `{n}` literal bytes it announces.
    pub async fn read_record(&mut self) -> Result<(String, Option<Vec<u8>>)> {
        let line = self.read_line().await?;
        if let Some(n) = parse_literal_len(&line) {
            if n > 4 * 1024 * 1024 {
                return Err(err("literal too large"));
            }
            let data = self.read_bytes(n).await?;
            return Ok((line, Some(data)));
        }
        Ok((line, None))
    }

    pub async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        self.stream
            .write_all(data)
            .await
            .map_err(|e| err(format!("write failed: {}", e)))
    }

    pub async fn write_line(&mut self, line: &str) -> Result<()> {
        let mut data = Vec::with_capacity(line.len() + 2);
        data.extend_from_slice(line.as_bytes());
        data.extend_from_slice(b"\r\n");
        self.write_raw(&data).await
    }

    /// SMTP reply: keep reading lines until `NNN ` (code + space).
    pub async fn read_smtp_reply(&mut self) -> Result<(u16, Vec<String>)> {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line().await?;
            let lb = line.as_bytes();
            let is_last = lb.len() >= 4 && lb[3] == b' ' && lb[..3].iter().all(|c| c.is_ascii_digit());
            lines.push(line);
            if is_last {
                break;
            }
        }
        let code: u16 = lines
            .last()
            .unwrap()[..3]
            .parse()
            .map_err(|_| err("bad smtp reply code"))?;
        Ok((code, lines))
    }

    pub async fn shutdown(&mut self) {
        let _ = self.stream.shutdown().await;
    }
}

fn parse_literal_len(line: &str) -> Option<usize> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if !line.ends_with('}') {
        return None;
    }
    let start = line.rfind('{')?;
    line[start + 1..line.len() - 1].parse().ok()
}

// ---------------------------------------------------------------------------
// RFC 2047 encoded-word decoding: =?charset?B|Q?text?=
// ---------------------------------------------------------------------------

fn decode_charset(charset: &str, raw: &[u8]) -> Option<String> {
    let cs = charset.trim().to_lowercase();
    let label = match cs.as_str() {
        "gb2312" | "gbk" | "gb18030" => "gb18030",
        other => other,
    };
    let enc = encoding_rs::Encoding::for_label(label.as_bytes())?;
    let (s, _, _) = enc.decode(raw);
    Some(s.into_owned())
}

fn parse_encoded_word(b: &[u8], start: usize) -> Option<(String, usize)> {
    // start points at '=', expect "=?charset?X?text?="
    if start + 2 >= b.len() || b[start + 1] != b'?' {
        return None;
    }
    let mut j = start + 2;
    let cs_start = j;
    while j < b.len() && b[j] != b'?' {
        j += 1;
    }
    if j >= b.len() {
        return None;
    }
    let charset = std::str::from_utf8(&b[cs_start..j]).ok()?;
    j += 1; // skip '?'
    if j >= b.len() {
        return None;
    }
    let enc = b[j];
    if enc != b'B' && enc != b'b' && enc != b'Q' && enc != b'q' {
        return None;
    }
    j += 1;
    if j >= b.len() || b[j] != b'?' {
        return None;
    }
    j += 1;
    let text_start = j;
    while j + 1 < b.len() && !(b[j] == b'?' && b[j + 1] == b'=') {
        j += 1;
    }
    if j + 1 >= b.len() {
        return None;
    }
    let text = &b[text_start..j];
    let end = j + 2;

    let raw: Vec<u8> = if enc == b'B' || enc == b'b' {
        let t: Vec<u8> = text
            .iter()
            .filter(|c| !c.is_ascii_whitespace())
            .copied()
            .collect();
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(&t).ok()?
    } else {
        let mut v = Vec::with_capacity(text.len());
        let mut k = 0;
        while k < text.len() {
            if text[k] == b'_' {
                v.push(b' ');
                k += 1;
            } else if text[k] == b'=' && k + 3 <= text.len() {
                let h = std::str::from_utf8(&text[k + 1..k + 3]).ok()?;
                v.push(u8::from_str_radix(h, 16).ok()?);
                k += 3;
            } else {
                v.push(text[k]);
                k += 1;
            }
        }
        v
    };

    Some((decode_charset(charset, &raw)?, end))
}

pub fn decode_rfc2047(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' && i + 1 < b.len() && b[i + 1] == b'?' {
            if let Some((decoded, next)) = parse_encoded_word(b, i) {
                out.push_str(&decoded);
                i = next;
                continue;
            }
        }
        // `i` is always on a char boundary (we advance by char width,
        // and the ASCII "=?" scan never splits a multi-byte char).
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn parse_headers(raw: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    let mut last: Option<String> = None;
    for line in raw.split("\r\n") {
        if (line.starts_with(' ') || line.starts_with('\t')) && last.is_some() {
            let k = last.clone().unwrap();
            let v = headers.get(&k).cloned().unwrap_or_default();
            headers.insert(k, format!("{} {}", v, line.trim()));
        } else if let Some(colon) = line.find(':') {
            if colon > 0 {
                let k = line[..colon].trim().to_lowercase();
                headers.insert(k.clone(), line[colon + 1..].trim().to_string());
                last = Some(k);
            }
        }
    }
    headers
}

fn parse_uid(line: &str) -> Option<u32> {
    let mut pos = line.find("UID ")? + 4;
    let start = pos;
    let lb = line.as_bytes();
    while pos < lb.len() && lb[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos == start {
        return None;
    }
    line[start..pos].parse().ok()
}

// ---------------------------------------------------------------------------
// IMAP client
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct MsgMeta {
    pub uid: u32,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: String,
}

#[derive(serde::Serialize)]
pub struct FullMsg {
    pub uid: u32,
    pub size: usize,
    pub truncated: bool,
    pub raw: String,
}

pub struct ImapClient {
    reader: BufReader,
    tag: u32,
}

impl ImapClient {
    pub async fn open(host: &str, port: u16, user: &str, pass: &str) -> Result<Self> {
        let mut reader = BufReader::connect(host, port).await?;
        let greet = reader.read_line().await?;
        if !greet.starts_with("* OK") {
            let preview: String = greet.chars().take(80).collect();
            return Err(err(format!("bad greeting: {}", preview)));
        }
        let mut c = Self { reader, tag: 0 };
        // Quoted-string LOGIN. QQ auth codes are alphanumeric; the mailbox
        // address is plain ASCII, so this matches the previous client.
        // (Never log `user`/`pass`: errors below never interpolate them.)
        c.cmd("LOGIN", &format!("\"{}\" \"{}\"", user, pass)).await?;
        Ok(c)
    }

    /// Send a tagged command; returns the untagged records.
    async fn cmd(&mut self, name: &str, args: &str) -> Result<Vec<(String, Option<Vec<u8>>)>> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        let line = if args.is_empty() {
            format!("{} {}", tag, name)
        } else {
            format!("{} {} {}", tag, name, args)
        };
        self.reader.write_line(&line).await?;
        let mut untagged = Vec::new();
        loop {
            let (rl, lit) = self.reader.read_record().await?;
            if rl.starts_with(&format!("{} ", tag)) {
                if !rl.starts_with(&format!("{} OK", tag)) {
                    let preview: String = rl.chars().take(120).collect();
                    return Err(err(format!("{} failed: {}", name, preview)));
                }
                return Ok(untagged);
            }
            untagged.push((rl, lit));
        }
    }

    pub async fn select(&mut self, mailbox: &str) -> Result<u32> {
        let recs = self.cmd("SELECT", &format!("\"{}\"", mailbox)).await?;
        let mut exists = 0;
        for (line, _) in &recs {
            if let Some(rest) = line.strip_prefix("* ") {
                let mut parts = rest.split_whitespace();
                if let (Some(n), Some("EXISTS")) = (parts.next(), parts.next()) {
                    if let Ok(v) = n.parse() {
                        exists = v;
                    }
                }
            }
        }
        Ok(exists)
    }

    pub async fn uid_search(&mut self, criteria: &str) -> Result<Vec<u32>> {
        let recs = self.cmd("UID SEARCH", criteria).await?;
        let mut uids = Vec::new();
        for (line, _) in &recs {
            if let Some(rest) = line.strip_prefix("* SEARCH") {
                for tok in rest.split_whitespace() {
                    if let Ok(v) = tok.parse::<u32>() {
                        uids.push(v);
                    }
                }
            }
        }
        Ok(uids)
    }

    /// Headers for the newest `limit` uids, newest first.
    pub async fn fetch_headers(&mut self, uids: &[u32], limit: usize) -> Result<Vec<MsgMeta>> {
        // uids arrive ascending; take the tail (= newest) in reverse order.
        let pick: Vec<u32> = uids.iter().rev().take(limit).copied().collect();
        if pick.is_empty() {
            return Ok(Vec::new());
        }
        let set = pick
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let recs = self
            .cmd(
                "UID FETCH",
                &format!("{} (UID BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)])", set),
            )
            .await?;
        let mut out = Vec::new();
        for (line, lit) in &recs {
            let uid = match parse_uid(line) {
                Some(u) => u,
                None => continue,
            };
            let lit = match lit {
                Some(b) => b,
                None => continue,
            };
            let h = parse_headers(&String::from_utf8_lossy(lit));
            let get = |k: &str| h.get(k).map(|s| s.as_str()).unwrap_or("");
            out.push(MsgMeta {
                uid,
                from: decode_rfc2047(get("from")),
                to: decode_rfc2047(get("to")),
                subject: decode_rfc2047(get("subject")),
                date: get("date").to_string(),
            });
        }
        out.sort_by(|a, b| b.uid.cmp(&a.uid));
        Ok(out)
    }

    pub async fn fetch_full(&mut self, uid: u32, max_bytes: usize) -> Result<FullMsg> {
        let recs = self
            .cmd("UID FETCH", &format!("{} (UID BODY.PEEK[])", uid))
            .await?;
        for (line, lit) in &recs {
            if parse_uid(line) == Some(uid) {
                if let Some(data) = lit {
                    let size = data.len();
                    let truncated = size > max_bytes;
                    let raw =
                        String::from_utf8_lossy(&data[..data.len().min(max_bytes)]).into_owned();
                    return Ok(FullMsg {
                        uid,
                        size,
                        truncated,
                        raw,
                    });
                }
            }
        }
        Err(err(format!("message not found: uid {}", uid)))
    }

    pub async fn close(&mut self) {
        let _ = self.cmd("LOGOUT", "").await;
        self.reader.shutdown().await;
    }
}
