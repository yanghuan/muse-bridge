#!/usr/bin/env python3
"""Sandbox-side client for muse-bridge (QQ Mail over Cloudflare Worker).

Config: ~/.config/muse-bridge/env  (0600)
    BRIDGE_URL=https://muse-bridge.<you>.workers.dev
    BRIDGE_TOKEN=<random token>

Usage:
    qqmail.py list [--unseen] [--limit N] [--mailbox INBOX]
    qqmail.py search 'SUBJECT "hello"'
    qqmail.py fetch UID
    qqmail.py send --to a@b.com --subject 'hi' --body 'hello'
"""
import json
import os
import sys
import urllib.request

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


def api(path, payload):
    cfg = load_config()
    req = urllib.request.Request(
        cfg['BRIDGE_URL'].rstrip('/') + path,
        data=json.dumps(payload).encode(),
        headers={
            'Content-Type': 'application/json',
            'Authorization': 'Bearer ' + cfg['BRIDGE_TOKEN'],
        },
        method='POST',
    )
    # Uses https_proxy from environment (required in sandboxed egress).
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


def cmd_list(args):
    import argparse
    p = argparse.ArgumentParser()
    p.add_argument('--unseen', action='store_true')
    p.add_argument('--limit', type=int, default=20)
    p.add_argument('--mailbox', default='INBOX')
    a = p.parse_args(args)
    r = api('/imap', {'action': 'list', 'mailbox': a.mailbox, 'limit': a.limit, 'unseen': a.unseen})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    print(f"total: {r['total']}, showing: {len(r['messages'])}")
    for m in r['messages']:
        print(f"{m['uid']:>8} | {m['date'][:31]:31s} | {m['from'][:40]:40s} | {m['subject'][:60]}")


def cmd_search(args):
    criteria = args[0] if args else 'ALL'
    r = api('/imap', {'action': 'search', 'criteria': criteria, 'limit': 20})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    print(f"total: {r['total']}, showing: {len(r['messages'])}")
    for m in r['messages']:
        print(f"{m['uid']:>8} | {m['date'][:31]:31s} | {m['from'][:40]:40s} | {m['subject'][:60]}")


def cmd_fetch(args):
    uid = int(args[0])
    r = api('/imap', {'action': 'fetch', 'uid': uid})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    m = r['message']
    print(f"uid={m['uid']} size={m['size']}" + (' (truncated)' if m['truncated'] else ''))
    print(m['raw'])


def cmd_send(args):
    import argparse
    p = argparse.ArgumentParser()
    p.add_argument('--to', required=True)
    p.add_argument('--subject', default='')
    p.add_argument('--body', default='')
    p.add_argument('--html', default='')
    a = p.parse_args(args)
    to = [t.strip() for t in a.to.split(',')]
    r = api('/smtp', {'to': to if len(to) > 1 else to[0],
                      'subject': a.subject, 'text': a.body, 'html': a.html})
    if not r.get('ok'):
        sys.exit('error: ' + r.get('error', 'unknown'))
    print('sent, message-id:', r.get('messageId'))


CMDS = {'list': cmd_list, 'search': cmd_search, 'fetch': cmd_fetch, 'send': cmd_send}

if __name__ == '__main__':
    if len(sys.argv) < 2 or sys.argv[1] not in CMDS:
        sys.exit('usage: qqmail.py {' + '|'.join(CMDS) + '} ...')
    if not os.path.exists(CONFIG):
        sys.exit(f'config missing: {CONFIG} (create it with BRIDGE_URL and BRIDGE_TOKEN, chmod 600)')
    CMDS[sys.argv[1]](sys.argv[2:])
