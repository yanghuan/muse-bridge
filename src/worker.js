import { ImapClient, MailError } from './imap.js';
import { SmtpClient } from './smtp.js';

const QQ_IMAP = { hostname: 'imap.qq.com', port: 993 };
const QQ_SMTP = { hostname: 'smtp.qq.com', port: 587, starttls: true };

function json(data, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { 'content-type': 'application/json; charset=utf-8' },
  });
}

// Constant-time-ish bearer check (avoid early-exit oracle).
function checkAuth(req, env) {
  const hdr = req.headers.get('authorization') || '';
  const want = 'Bearer ' + (env.BRIDGE_TOKEN || '');
  if (hdr.length !== want.length) return false;
  let d = 0;
  for (let i = 0; i < hdr.length; i++) d |= hdr.charCodeAt(i) ^ want.charCodeAt(i);
  return d === 0;
}

async function handleImap(body, env) {
  const { action = 'list', mailbox = 'INBOX', limit = 20, unseen = false, criteria = '', uid } = body || {};
  const client = await ImapClient.open(QQ_IMAP.hostname, QQ_IMAP.port, env.QQ_USER, env.QQ_AUTH_CODE);
  try {
    await client.select(mailbox);
    if (action === 'fetch') {
      if (!uid) throw new MailError('fetch requires uid');
      const msg = await client.fetchFull(Number(uid));
      return json({ ok: true, message: msg });
    }
    let uids;
    if (action === 'search') {
      uids = await client.uidSearch(criteria || 'ALL');
    } else {
      // list
      uids = await client.uidSearch(unseen ? 'UNSEEN' : 'ALL');
    }
    const messages = await client.fetchHeaders(uids, Math.min(Number(limit) || 20, 100));
    return json({ ok: true, total: uids.length, messages });
  } finally {
    await client.close();
  }
}

async function handleSmtp(body, env) {
  const { to, subject = '', text = '', html = '' } = body || {};
  if (!to) throw new MailError('smtp requires "to"');
  const client = await SmtpClient.open(QQ_SMTP.hostname, QQ_SMTP.port, env.QQ_USER, env.QQ_AUTH_CODE, QQ_SMTP.starttls);
  try {
    const r = await client.send({ from: env.QQ_USER, to, subject, text, html });
    return json({ ok: true, messageId: r.messageId });
  } finally {
    await client.close();
  }
}

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (req.method === 'GET' && url.pathname === '/') {
      return json({ ok: true, service: 'muse-bridge', endpoints: ['POST /imap', 'POST /smtp'] });
    }
    if (req.method !== 'POST' || (url.pathname !== '/imap' && url.pathname !== '/smtp')) {
      return json({ ok: false, error: 'not found' }, 404);
    }
    if (!env.BRIDGE_TOKEN) return json({ ok: false, error: 'server misconfigured: BRIDGE_TOKEN missing' }, 500);
    if (!checkAuth(req, env)) return json({ ok: false, error: 'unauthorized' }, 401);
    if (!env.QQ_USER || !env.QQ_AUTH_CODE) {
      return json({ ok: false, error: 'server misconfigured: QQ credentials missing' }, 500);
    }
    let body = {};
    try {
      body = await req.json();
    } catch {
      return json({ ok: false, error: 'invalid JSON body' }, 400);
    }
    try {
      if (url.pathname === '/imap') return await handleImap(body, env);
      return await handleSmtp(body, env);
    } catch (e) {
      // Debug: include raw message (credential material is never interpolated into errors by this codebase).
      const msg = e instanceof Error ? e.message : String(e);
      return json({ ok: false, error: msg.slice(0, 300) }, 502);
    }
  },
};
