# muse-bridge

Stateless QQ Mail bridge: a single musl-static Rust binary exposing QQ Mail
IMAP (read) and SMTP (send) as an HTTPS JSON API.

**Design:** the server stores no mailbox credentials. Every request carries its
own `account: {user, auth_code}`, so any number of QQ mailboxes can be served.
The only server-side secret is `BRIDGE_TOKEN`, which authorizes API callers
(without it this would be an open SMTP relay).

## Build

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/muse-bridge  (single static binary)
```

Pure-Rust TLS (rustls, no OpenSSL), so the musl static build just works.

## Run

```sh
BRIDGE_TOKEN=$(openssl rand -hex 32) LISTEN_ADDR=127.0.0.1:8080 ./muse-bridge
```

Systemd: see `deploy/muse-bridge.service` + `deploy/env.example`.

## API

All endpoints require `Authorization: Bearer <BRIDGE_TOKEN>`.

### POST /imap

```json
{
  "account": {"user": "a@qq.com", "auth_code": "<QQ 授权码>"},
  "action": "list | search | fetch",
  "mailbox": "INBOX",
  "limit": 20,
  "unseen": false,
  "criteria": "SUBJECT \"hello\"",
  "uid": 123
}
```

- `list`: newest `limit` messages (headers only), `total` = mailbox count.
- `search`: IMAP SEARCH criteria, e.g. `SUBJECT "x"`, `FROM "y"`, `UNSEEN`.
- `fetch`: full raw message by UID (truncated at 256 KiB, `truncated` flag set).

### POST /smtp

```json
{
  "account": {"user": "a@qq.com", "auth_code": "<QQ 授权码>"},
  "from": "a@qq.com",
  "to": ["b@example.com"],
  "subject": "hi",
  "text": "hello",
  "html": "<p>hello</p>"
}
```

`from` defaults to `account.user`; `to` accepts a string or an array.

## Client

`client/qqmail.py` — sandbox-side CLI. Config `~/.config/muse-bridge/env` (0600):

```
BRIDGE_URL=https://bridge.example.com
BRIDGE_TOKEN=<token>
QQ_USER=a@qq.com
QQ_AUTH_CODE=<code>
QQ_USER_2=b@qq.com
QQ_AUTH_CODE_2=<code>
```

```sh
qqmail.py list
qqmail.py -a b@qq.com list --unseen --limit 10
qqmail.py search 'SUBJECT "账单"'
qqmail.py fetch 12345
qqmail.py send --to x@y.com --subject hi --body hello
```

## Notes

- QQ 邮箱授权码在 mail.qq.com 设置 → 账户 → POP3/IMAP/SMTP 开启后生成（不是 QQ 密码）。
- The server never logs request bodies or credentials; mail errors are
  returned to the caller only.
- Supersedes the earlier Cloudflare Worker prototype (removed): Workers
  cannot open outbound SMTP connections, so sending was impossible there.
