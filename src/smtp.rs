//! Minimal SMTP submission client (implicit TLS, AUTH LOGIN).
//!
//! Uses the shared persistent-buffer [`BufReader`](crate::imap::BufReader),
//! so multi-line replies (EHLO's `250-...` block) are never truncated.

use crate::imap::{err, BufReader, Result};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct SmtpClient {
    reader: BufReader,
}

fn b64(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

/// RFC 2047 for non-ASCII subjects.
fn encode_subject(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    use base64::Engine;
    format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    )
}

fn now_rfc2822() -> String {
    use time::format_description::well_known::Rfc2822;
    time::OffsetDateTime::now_utc()
        .format(&Rfc2822)
        .unwrap_or_default()
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}.{}", nanos, c)
}

impl SmtpClient {
    pub async fn open(host: &str, port: u16, user: &str, pass: &str) -> Result<Self> {
        let mut reader = BufReader::connect(host, port).await?;
        let (code, _) = reader.read_smtp_reply().await?;
        if code != 220 {
            return Err(err(format!("SMTP greeting failed ({})", code)));
        }
        let mut c = Self { reader };
        c.cmd("EHLO muse-bridge", &[250]).await?;
        // AUTH LOGIN. (Never log `user`/`pass`: errors never interpolate them.)
        c.cmd("AUTH LOGIN", &[334]).await?;
        c.cmd(&b64(user), &[334]).await?;
        c.cmd(&b64(pass), &[235]).await?;
        Ok(c)
    }

    async fn cmd(&mut self, line: &str, expect: &[u16]) -> Result<Vec<String>> {
        self.reader.write_line(line).await?;
        let (code, lines) = self.reader.read_smtp_reply().await?;
        if !expect.contains(&code) {
            let verb = line.split_whitespace().next().unwrap_or("?");
            let detail: String = lines.join(" | ").chars().take(160).collect();
            return Err(err(format!("SMTP {} failed ({}): {}", verb, code, detail)));
        }
        Ok(lines)
    }

    /// Send one message. Returns the generated Message-ID.
    pub async fn send(
        &mut self,
        from: &str,
        to: &[String],
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<String> {
        let date = now_rfc2822();
        let message_id = format!("<{}@muse-bridge>", unique_suffix());
        let mut headers = format!(
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: {}\r\nMIME-Version: 1.0\r\n",
            from,
            to.join(", "),
            encode_subject(subject),
            date,
            message_id
        );
        let body = if html.is_empty() {
            headers.push_str("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n");
            format!("{}\r\n", text)
        } else {
            let boundary = format!("musebridge{}", unique_suffix().replace('.', ""));
            headers.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{}\"\r\n",
                boundary
            ));
            format!(
                "--{b}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{text}\r\n\
                 --{b}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{html}\r\n\
                 --{b}--\r\n",
                b = boundary,
                text = text,
                html = html
            )
        };
        // Dot-stuffing, then terminate DATA with <CRLF>.<CRLF>.
        let data = format!("{}\r\n{}", headers, body).replace("\r\n.", "\r\n..");

        self.cmd(&format!("MAIL FROM:<{}>", from), &[250]).await?;
        for rcpt in to {
            self.cmd(&format!("RCPT TO:<{}>", rcpt), &[250, 251])
                .await?;
        }
        self.cmd("DATA", &[354]).await?;
        self.reader
            .write_raw(format!("{}\r\n.\r\n", data).as_bytes())
            .await?;
        let (code, _) = self.reader.read_smtp_reply().await?;
        if code != 250 {
            return Err(err(format!("SMTP DATA failed ({})", code)));
        }
        Ok(message_id)
    }

    pub async fn close(&mut self) {
        let _ = self.cmd("QUIT", &[221]).await;
        self.reader.shutdown().await;
    }
}
