# muse-bridge

Cloudflare Worker bridging QQ Mail IMAP/SMTP over an HTTPS JSON API.

**Why:** some sandboxed environments can't reach `imap.qq.com` / `smtp.qq.com`
directly (egress proxy blocks the hosts; direct TLS is intercepted), while
`*.workers.dev` over HTTPS works fine. This Worker accepts JSON over HTTPS and
speaks raw IMAP/SMTP to QQ Mail on the other side via the Workers Sockets API.

## Architecture

```
sandbox --HTTPS/JSON--> Worker --TCP+TLS--> imap.qq.com:993 / smtp.qq.com:465
```

- `POST /imap` — list / search / fetch messages (IMAP)
- `POST /smtp` — send a message (SMTP submission)
- Stateless per request: one TCP connection per call, closed afterwards.
  No IMAP IDLE; poll instead.

## Setup

### 1. QQ Mail: enable IMAP/SMTP and get an authorization code

1. Log in to https://mail.qq.com
2. 设置 → 账户 → POP3/IMAP/SMTP/Exchange/CardDAV/CalDAV服务
3. Enable **IMAP/SMTP服务**, verify via SMS, and generate an **授权码**
   (this is NOT your QQ password — it's a 16-char token for third-party clients).

### 2. Deploy the Worker

```bash
npm install
npx wrangler login
npx wrangler secret put BRIDGE_TOKEN   # random token, used as API bearer
npx wrangler secret put QQ_USER        # e.g. sy.yanghuan@qq.com
npx wrangler secret put QQ_AUTH_CODE   # the 16-char authorization code
npx wrangler deploy
```

Note the assigned URL, e.g. `https://muse-bridge.<you>.workers.dev`.

### 3. Sandbox client config

```bash
mkdir -p ~/.config/muse-bridge && chmod 700 ~/.config/muse-bridge
cat > ~/.config/muse-bridge/env <<'EOF'
BRIDGE_URL=https://muse-bridge.<you>.workers.dev
BRIDGE_TOKEN=<same token as above>
EOF
chmod 600 ~/.config/muse-bridge/env
```

HTTPS goes through `https_proxy` from the environment (required for sandboxed egress).

## API

All endpoints require `Authorization: Bearer <BRIDGE_TOKEN>`.

### POST /imap

```jsonc
// list recent (or unseen) messages
{ "action": "list", "mailbox": "INBOX", "limit": 20, "unseen": false }

// search with raw IMAP criteria
{ "action": "search", "criteria": "FROM \"no-reply@accounts.google.com\"", "limit": 20 }

// fetch one full message by UID (raw RFC822, capped at 256KB)
{ "action": "fetch", "uid": 12345 }
```

Response: `{ "ok": true, "total": N, "messages": [{ "uid", "from", "to", "subject", "date" }] }`
or `{ "ok": true, "message": { "uid", "size", "truncated", "raw" } }`.

### POST /smtp

```jsonc
{
  "to": "a@b.com",            // or ["a@b.com", "c@d.com"]
  "subject": "hello",
  "text": "plain body",       // optional if html given
  "html": "<p>html body</p>"  // optional
}
```

Response: `{ "ok": true, "messageId": "..." }`.

Errors: `{ "ok": false, "error": "..." }` (credential material is never echoed).

## Client usage

```bash
python3 client/qqmail.py list --unseen --limit 10
python3 client/qqmail.py search 'SUBJECT "安全提醒"'
python3 client/qqmail.py fetch 12345
python3 client/qqmail.py send --to a@b.com --subject 'hi' --body 'hello'
```

## Known limitations

- SMTP submission via Workers (port 465) has scattered real-world failure
  reports (`proxy request failed` / TLS handshake failures on deploy while
  `wrangler dev` works). **Test `/smtp` right after deploy**; if QQ's 465
  fails, try switching the Worker to port 587 + STARTTLS.
- Request/response only — no push/IDLE. Poll on whatever cadence you need.
- The bridge token is a bearer secret: anyone holding it can read/send your
  QQ mail. Rotate with `wrangler secret put BRIDGE_TOKEN` if leaked.
