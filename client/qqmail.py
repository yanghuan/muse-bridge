#!/usr/bin/env python3
"""Sandbox-side client for muse-bridge (stateless QQ Mail bridge).

The server stores no mailbox credentials: every request carries its own
account, so any number of QQ mailboxes is supported.

Config: ~/.config/muse-bridge/env  (0600)
    BRIDGE_URL=https://bridge.example.com
    BRIDGE_TOKEN=<random token>
    QQ_USER=sy.yanghuan@qq.com
    QQ_AUTH_CODE=<authorization code for QQ_USER, not the QQ password>
    # additional mailboxes:
    QQ_USER_2=other@qq.com
    QQ_AUTH_CODE_2=<code for QQ_USER_2>

Usage:
    qqmail.py [-a account@qq.com] list [--unseen] [--limit N] [--mailbox INBOX]
    qqmail.py [-a account@qq.com] search 'SUBJECT "hello"'
    qqmail.py [-a account@qq.com] fetch UID
    qqmail.py [-a account@qq.com] send --to a@b.com [--from me@qq.com] --subject 'hi' --body 'hello'
"""
import argparse
import json
import os
import subprocess
import sys

CONFIG = os.path.expanduser('~/.config/muse-bridge/env')


def load_config():
    cfg = {}
    with open(CONFIG) as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith('#') and '=' in line:
                k, v = line.split('=', 1)
                cfg[k.strip()] = v.strip()
    return cfg


def find_accounts(cfg):
    """Map mailbox address -> auth code from QQ_USER[_N]/QQ_AUTH_CODE[_N] pairs."""
    accounts = {}
    for k, v in cfg.items():
        if k == 'QQ_USER' or (k.startswith('QQ_USER_') and k[len('QQ_USER_'):].isdigit()):
            suffix = k[len('QQ_USER'):]  # '' or '_2'
            code = cfg.get('QQ_AUTH_CODE' + suffix)
            if v and code:
                accounts[v] = code
    return accounts


def resolve_account(cfg, name):
    accounts = find_accounts(cfg)
    if not accounts:
        sys.exit('no QQ accounts in config (need QQ_USER/QQ_AUTH_CODE pairs)')
    if name is None:
        name = cfg.get('QQ_USER') or sorted(accounts)[0]
    code = accounts.get(name)
    if not code:
        sys.exit(f'account not configured: {name} (known: {", ".join(sorted(accounts))})')
    return {'user': name, 'auth_code': code}


def api(path, payload):
    cfg = load_config()
    p = subprocess.run(
        ['curl', '-s', '--max-time', '90',
         '-X', 'POST', cfg['BRIDGE_URL'].rstrip('/') + path,
         '-H', 'Content-Type: application/json',
         '-H', 'Authorization: Bearer ' + cfg['BRIDGE_TOKEN'],
         '-d', json.dumps(payload)],
        capture_output=True, text=True, timeout=100,
    )
    if p.returncode != 0:
        sys.exit('request failed: ' + p.stderr.strip()[:120])
    try:
        return json.loads(p.stdout)
    except json.JSONDecodeError:
        sys.exit('bad response: ' + p.stdout[:120])


def cmd_list(args, account):
    p = argparse.ArgumentParser()
    p.add_argument('--unseen', action='store_true')
    p.add_argument('--limit', type=int, default=20)
    p.add_argument('--mailbox', default='INBOX')
    a = p.parse_args(args)
    r = api('/imap', {'account': account, 'action': 'list',
                      'mailbox': a.mailbox, 'limit': a.limit, 'unseen': a.unseen})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    print(f"total: {r['total']}, showing: {len(r['messages'])}")
    for m in r['messages']:
        print(f"{m['uid']:>8} | {m['date'][:31]:31s} | {m['from'][:40]:40s} | {m['subject'][:60]}")


def cmd_search(args, account):
    criteria = args[0] if args else 'ALL'
    r = api('/imap', {'account': account, 'action': 'search', 'criteria': criteria, 'limit': 20})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    print(f"total: {r['total']}, showing: {len(r['messages'])}")
    for m in r['messages']:
        print(f"{m['uid']:>8} | {m['date'][:31]:31s} | {m['from'][:40]:40s} | {m['subject'][:60]}")


def cmd_fetch(args, account):
    uid = int(args[0])
    r = api('/imap', {'account': account, 'action': 'fetch', 'uid': uid})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    m = r['message']
    print(f"uid={m['uid']} size={m['size']}" + (' (truncated)' if m['truncated'] else ''))
    print(m['raw'])


def cmd_send(args, account):
    p = argparse.ArgumentParser()
    p.add_argument('--to', required=True)
    p.add_argument('--from', dest='frm', default=None)
    p.add_argument('--subject', default='')
    p.add_argument('--body', default='')
    p.add_argument('--html', default='')
    a = p.parse_args(args)
    to = [t.strip() for t in a.to.split(',')]
    payload = {'account': account,
               'to': to if len(to) > 1 else to[0],
               'subject': a.subject, 'text': a.body, 'html': a.html}
    if a.frm:
        payload['from'] = a.frm
    r = api('/smtp', payload)
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    print('sent, message-id:', r.get('messageId'))


CMDS = {'list': cmd_list, 'search': cmd_search, 'fetch': cmd_fetch, 'send': cmd_send}

if __name__ == '__main__':
    g = argparse.ArgumentParser()
    g.add_argument('-a', '--account', default=None, help='QQ mailbox address (default: QQ_USER)')
    g.add_argument('cmd', choices=list(CMDS))
    g.add_argument('rest', nargs=argparse.REMAINDER)
    a = g.parse_args()
    if not os.path.exists(CONFIG):
        sys.exit(f'config missing: {CONFIG} (create it with BRIDGE_URL, BRIDGE_TOKEN and QQ_USER/QQ_AUTH_CODE, chmod 600)')
    cfg = load_config()
    account = resolve_account(cfg, a.account)
    CMDS[a.cmd](a.rest, account)
