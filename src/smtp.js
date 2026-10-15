import { connect } from 'cloudflare:sockets';
import { MailError } from './imap.js';

// Minimal SMTP submission client (implicit TLS, AUTH LOGIN).
export class SmtpClient {
  constructor(sock, reader, writer) {
    this.sock = sock;
    this.reader = reader;
    this.writer = writer;
  }

  static async open(host, port, user, pass, starttls = false) {
    let sock = connect({ hostname: host, port }, { secureTransport: starttls ? 'starttls' : 'on' });
    let reader = sock.readable.getReader();
    let writer = sock.writable.getWriter();
    let c = new SmtpClient(sock, reader, writer);
    const greet = await c._readReply();
    if (greet.code !== 220) throw new MailError('SMTP greeting failed (' + greet.code + ')');
    await c._cmd(`EHLO muse-bridge`, [250]);
    if (starttls) {
      await c._cmd('STARTTLS', [220]);
      sock = sock.startTls();
      reader = sock.readable.getReader();
      writer = sock.writable.getWriter();
      c = new SmtpClient(sock, reader, writer);
      await c._cmd(`EHLO muse-bridge`, [250]);
    }
    await c._cmd('AUTH LOGIN', [334]);
    await c._cmd(b64(user), [334]);
    await c._cmd(b64(pass), [235]);
    return c;
  }

  async _readLine() {
    let buf = new Uint8Array(0);
    for (;;) {
      for (let i = 0; i + 1 < buf.length; i++) {
        if (buf[i] === 13 && buf[i + 1] === 10) {
          const line = new TextDecoder().decode(buf.slice(0, i));
          return line;
        }
      }
      const { done, value } = await this.reader.read();
      if (done) throw new MailError('SMTP connection closed');
      const nb = new Uint8Array(buf.length + value.length);
      nb.set(buf); nb.set(value, buf.length);
      buf = nb;
    }
  }

  // Read possibly multi-line reply, return {code, lines}
  async _readReply() {
    const lines = [];
    for (;;) {
      const line = await this._readLine();
      lines.push(line);
      if (/^\d{3} /.test(line)) break;
    }
    return { code: parseInt(lines[lines.length - 1].slice(0, 3), 10), lines };
  }

  async _cmd(line, expect) {
    await this.writer.write(new TextEncoder().encode(line + '\r\n'));
    const r = await this._readReply();
    if (!expect.includes(r.code)) {
      throw new MailError(`SMTP ${line.split(' ')[0]} failed (${r.code}): ` + r.lines.join(' | ').slice(0, 160));
    }
    return r;
  }

  static encodeSubject(s) {
    // RFC 2047 for non-ASCII subjects
    if (/^[\x00-\x7F]*$/.test(s)) return s;
    const bytes = new TextEncoder().encode(s);
    const bin = String.fromCharCode(...bytes);
    return '=?UTF-8?B?' + btoa(bin) + '?=';
  }

  async send({ from, to, subject, text, html }) {
    const toList = Array.isArray(to) ? to : [to];
    const date = new Date().toUTCString();
    const msgId = `<${Date.now()}.${Math.random().toString(36).slice(2)}@muse-bridge>`;
    let headers =
      `From: ${from}\r\n` +
      `To: ${toList.join(', ')}\r\n` +
      `Subject: ${SmtpClient.encodeSubject(subject)}\r\n` +
      `Date: ${date}\r\n` +
      `Message-ID: ${msgId}\r\n` +
      `MIME-Version: 1.0\r\n`;
    let body;
    if (html) {
      const boundary = 'b' + Date.now().toString(36) + Math.random().toString(36).slice(2);
      headers += `Content-Type: multipart/alternative; boundary="${boundary}"\r\n`;
      body =
        `--${boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n${text || ''}\r\n` +
        `--${boundary}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n${html}\r\n` +
        `--${boundary}--\r\n`;
    } else {
      headers += `Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n`;
      body = (text || '') + '\r\n';
    }
    // Dot-stuffing
    const data = (headers + '\r\n' + body).replace(/\r\n\./g, '\r\n..');

    await this._cmd(`MAIL FROM:<${from}>`, [250]);
    for (const rcpt of toList) await this._cmd(`RCPT TO:<${rcpt}>`, [250, 251]);
    await this._cmd('DATA', [354]);
    await this.writer.write(new TextEncoder().encode(data + '\r\n.\r\n'));
    const r = await this._readReply();
    if (r.code !== 250) throw new MailError('SMTP DATA failed (' + r.code + ')');
    return { messageId: msgId };
  }

  async close() {
    try { await this._cmd('QUIT', [221]); } catch { /* ignore */ }
    try { this.sock.close(); } catch { /* ignore */ }
  }
}

function b64(s) {
  const bytes = new TextEncoder().encode(s);
  return btoa(String.fromCharCode(...bytes));
}
