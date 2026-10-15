import { connect } from 'cloudflare:sockets';

export class MailError extends Error {}

// Buffered byte reader over a ReadableStream, handles IMAP literals.
class BufReader {
  constructor(readable) {
    this.reader = readable.getReader();
    this.buf = new Uint8Array(0);
  }
  async _fill() {
    const { done, value } = await this.reader.read();
    if (done) throw new MailError('connection closed by server');
    const nb = new Uint8Array(this.buf.length + value.length);
    nb.set(this.buf);
    nb.set(value, this.buf.length);
    this.buf = nb;
  }
  async readLine() {
    for (;;) {
      for (let i = 0; i + 1 < this.buf.length; i++) {
        if (this.buf[i] === 13 && this.buf[i + 1] === 10) {
          const line = new TextDecoder().decode(this.buf.slice(0, i));
          this.buf = this.buf.slice(i + 2);
          return line;
        }
      }
      await this._fill();
    }
  }
  async readBytes(n) {
    while (this.buf.length < n) await this._fill();
    const out = this.buf.slice(0, n);
    this.buf = this.buf.slice(n);
    return out;
  }
  // Read one IMAP response "record": a line, plus any literal bytes it announces.
  async readRecord() {
    const line = await this.readLine();
    const m = line.match(/\{(\d+)\}\r?$/);
    if (m) {
      const n = parseInt(m[1], 10);
      if (n > 4 * 1024 * 1024) throw new MailError('literal too large');
      const data = await this.readBytes(n);
      return { line, literal: data };
    }
    return { line, literal: null };
  }
}

// Decode RFC 2047 encoded-words: =?charset?B|Q?text?=
function decodeHeader(s) {
  if (!s) return '';
  return s.replace(/=\?([^?]+)\?([bBqQ])\?([^?]*)\?=/g, (m, charset, enc, text) => {
    try {
      let bytes;
      if (enc.toUpperCase() === 'B') {
        const bin = atob(text.replace(/\s/g, ''));
        bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
      } else {
        const qp = text.replace(/_/g, ' ').replace(/=([0-9A-Fa-f]{2})/g, (_, h) =>
          String.fromCharCode(parseInt(h, 16))
        );
        bytes = new TextEncoder().encode(qp); // qp chars are latin1 here; re-encode raw
        // Actually rebuild raw bytes properly:
        bytes = Uint8Array.from(qp, (c) => c.charCodeAt(0) & 0xff);
      }
      const cs = charset.trim().toLowerCase();
      const label = cs === 'gb2312' || cs === 'gbk' || cs === 'gb18030' ? 'gb18030' : cs;
      return new TextDecoder(label).decode(bytes);
    } catch {
      return m;
    }
  });
}

function parseHeaders(raw) {
  const headers = {};
  let last = null;
  for (const line of raw.split('\r\n')) {
    if (/^[ \t]/.test(line) && last) {
      headers[last] += ' ' + line.trim();
    } else {
      const i = line.indexOf(':');
      if (i > 0) {
        last = line.slice(0, i).trim().toLowerCase();
        headers[last] = line.slice(i + 1).trim();
      }
    }
  }
  return headers;
}

export class ImapClient {
  constructor(sock) {
    this.sock = sock;
    this.reader = new BufReader(sock.readable);
    this.writer = sock.writable.getWriter();
    this.tag = 0;
  }

  static async open(host, port, user, pass) {
    const sock = connect({ hostname: host, port }, { secureTransport: 'on' });
    const c = new ImapClient(sock);
    const greet = await c.reader.readLine();
    if (!greet.startsWith('* OK')) throw new MailError('bad greeting: ' + greet.slice(0, 80));
    await c.cmd('LOGIN', `"${user}" "${pass}"`);
    return c;
  }

  async _write(line) {
    await this.writer.write(new TextEncoder().encode(line + '\r\n'));
  }

  // Send tagged command, return {untagged: [...records], tagged: line}
  async cmd(name, args = '') {
    const tag = 'a' + ++this.tag;
    await this._write(`${tag} ${name}${args ? ' ' + args : ''}`);
    const untagged = [];
    for (;;) {
      const rec = await this.reader.readRecord();
      if (rec.line.startsWith(tag + ' ')) {
        if (!rec.line.startsWith(tag + ' OK')) throw new MailError(`${name} failed: ` + rec.line.slice(0, 120));
        return { untagged, tagged: rec.line };
      }
      untagged.push(rec);
    }
  }

  async select(mailbox) {
    const r = await this.cmd('SELECT', `"${mailbox}"`);
    let exists = 0;
    for (const rec of r.untagged) {
      const m = rec.line.match(/^\* (\d+) EXISTS/);
      if (m) exists = parseInt(m[1], 10);
    }
    return exists;
  }

  async uidSearch(criteria) {
    const r = await this.cmd('UID SEARCH', criteria);
    const uids = [];
    for (const rec of r.untagged) {
      const m = rec.line.match(/^\* SEARCH\s*(.*)$/);
      if (m && m[1].trim()) uids.push(...m[1].trim().split(/\s+/).map(Number));
    }
    return uids;
  }

  // Fetch headers for uids, newest first, up to limit.
  async fetchHeaders(uids, limit) {
    const pick = uids.slice(-limit).reverse();
    if (!pick.length) return [];
    const r = await this.cmd(
      'UID FETCH',
      `${pick.join(',')} (UID BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE)])`
    );
    const out = [];
    for (const rec of r.untagged) {
      const um = rec.line.match(/UID (\d+)/);
      if (!um || !rec.literal) continue;
      const h = parseHeaders(new TextDecoder().decode(rec.literal));
      out.push({
        uid: parseInt(um[1], 10),
        from: decodeHeader(h['from'] || ''),
        to: decodeHeader(h['to'] || ''),
        subject: decodeHeader(h['subject'] || ''),
        date: h['date'] || '',
      });
    }
    out.sort((a, b) => b.uid - a.uid);
    return out;
  }

  async fetchFull(uid, maxBytes = 256 * 1024) {
    const r = await this.cmd('UID FETCH', `${uid} (UID BODY.PEEK[])`);
    for (const rec of r.untagged) {
      const um = rec.line.match(/UID (\d+)/);
      if (um && parseInt(um[1], 10) === uid && rec.literal) {
        const data = rec.literal.slice(0, maxBytes);
        return { uid, size: rec.literal.length, truncated: rec.literal.length > maxBytes, raw: new TextDecoder().decode(data) };
      }
    }
    throw new MailError('message not found: uid ' + uid);
  }

  async close() {
    try {
      await this.cmd('LOGOUT');
    } catch { /* ignore */ }
    try { this.sock.close(); } catch { /* ignore */ }
  }
}
