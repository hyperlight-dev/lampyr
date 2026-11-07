#!/usr/bin/env python3
"""Check connection-style DNS under an exact Hyperlight hostname allowlist."""

import argparse
import json
from pathlib import Path
import socket
import subprocess
import tempfile


PROBE = r"""
const dns = require('node:dns');
const net = require('node:net');
const fs = require('node:fs');
const allowed = JSON.parse(process.env.PROBE_ALLOWED);
const denied = process.env.PROBE_DENIED;
const loopback = process.env.PROBE_LOOPBACK;
const loopbackUDP = process.env.PROBE_LOOPBACK_UDP;
async function check(name, run) {
  try { console.log(JSON.stringify({name, ok: true, value: await run()})); }
  catch (error) { console.log(JSON.stringify({name, ok: false, code: error.code, message: error.message})); }
}
(async () => {
  console.log(JSON.stringify({name:'resolver', ok:true, value:fs.readFileSync('/etc/resolv.conf','utf8')}));
  for (const host of allowed) {
    await check('A:'+host, () => dns.promises.resolve4(host));
    await check('lookup:'+host, () => dns.promises.lookup(host, {all:true}));
    await check('ADDRCONFIG:'+host, () => dns.promises.lookup(host, {all:true, hints:dns.ADDRCONFIG}));
  }
  const resolver = new dns.promises.Resolver({timeout:500, tries:1});
  await check('denied:'+denied, () => resolver.resolve4(denied));
  if (loopback) {
    await check('loopback-TCP', () => new Promise((resolve,reject) => {
      const socket = net.createConnection({host:'127.0.0.1', port:Number(loopback)});
      socket.setTimeout(1000, () => socket.destroy(new Error('loopback timeout')));
      socket.once('error', reject);
      socket.once('connect', () => {socket.destroy(); resolve('connected');});
    }));
  }
  if (loopbackUDP) {
    await check('loopback-UDP-payload', () => new Promise((resolve,reject) => {
      const socket = require('node:dgram').createSocket('udp4');
      let closed = false;
      function finish(error) {
        if (closed) return;
        closed = true;
        socket.close();
        if (error) reject(error); else resolve('sent');
      }
      socket.once('error', finish);
      socket.connect(Number(loopbackUDP), '127.0.0.1', error => {
        if (error) { finish(error); return; }
        try { socket.send(Buffer.from('lampyr-loopback-negative-control'), finish); }
        catch (error) { finish(error); }
      });
    }));
  }
  if (process.env.PROBE_TCP) {
    const host = allowed[0];
    await check('TCP:'+host, () => new Promise((resolve,reject) => {
      const socket = net.createConnection({host, port:Number(process.env.PROBE_TCP)});
      socket.setTimeout(5000, () => socket.destroy(new Error('TCP timeout')));
      socket.once('error', reject);
      socket.once('connect', () => {socket.destroy(); resolve('connected');});
    }));
  }
})().catch(error => {console.error(error); process.exitCode=1;});
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hluk", type=Path, required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--rootfs", type=Path)
    source.add_argument("--snapshot", type=Path)
    parser.add_argument("--allowed", action="append", required=True, help="Exact permitted DNS name; repeat for each")
    parser.add_argument("--denied", default="example.com")
    parser.add_argument("--resolv-conf", type=Path)
    parser.add_argument("--tcp-port", type=int, help="Also test ordinary connection-style lookup and TCP for first allowed name")
    parser.add_argument("--loopback-port", type=int, help="Verify guest TCP to a host loopback listener stays forbidden")
    parser.add_argument("--loopback-controls", action="store_true", help="Create host TCP/UDP loopback listeners and prove no guest payload reaches them")
    args = parser.parse_args()
    assert args.denied not in args.allowed, "negative control must not be authorized"
    command = [str(args.hluk.resolve())]
    if args.rootfs:
        command += ["run", "--initrd", str(args.rootfs.resolve()), "--scratch-mb", "512"]
    else:
        command += ["snapshot", "run", str(args.snapshot.resolve())]
    for host in args.allowed:
        command += ["--net-allow", host]
    if args.resolv_conf:
        command += ["--resolv-conf", str(args.resolv_conf.resolve())]
    command += ["--env", "PROBE_ALLOWED=" + json.dumps(args.allowed), "--env", "PROBE_DENIED=" + args.denied]
    if args.tcp_port:
        command += ["--env", "PROBE_TCP=" + str(args.tcp_port)]
    if args.loopback_port:
        command += ["--env", "PROBE_LOOPBACK=" + str(args.loopback_port)]
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as tcp, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        if args.loopback_controls:
            tcp.bind(("127.0.0.1", 0))
            tcp.listen(1)
            tcp.setblocking(False)
            udp.bind(("127.0.0.1", 0))
            udp.setblocking(False)
            command += [
                "--env", "PROBE_LOOPBACK=" + str(tcp.getsockname()[1]),
                "--env", "PROBE_LOOPBACK_UDP=" + str(udp.getsockname()[1]),
            ]
        with tempfile.TemporaryDirectory(prefix="lampyr-dns-contract-") as directory:
            script = Path(directory) / "probe.js"
            script.write_text(PROBE, encoding="utf-8")
            command.append(str(script))
            print("ARGV:", json.dumps(command), flush=True)
            completed = subprocess.run(command, text=True, capture_output=True, timeout=60, check=False)
        if args.loopback_controls:
            try:
                connection, _ = tcp.accept()
            except BlockingIOError:
                pass
            else:
                connection.close()
                raise AssertionError("guest reached host loopback TCP listener")
            try:
                udp.recvfrom(1024)
            except BlockingIOError:
                pass
            else:
                raise AssertionError("guest sent payload to host loopback UDP listener")
    print(completed.stdout, end="")
    print(completed.stderr, end="")
    assert completed.returncode == 0, f"runtime exited {completed.returncode}"
    results = [json.loads(line) for line in completed.stdout.splitlines() if line.startswith("{")]
    by_name = {result["name"]: result for result in results}
    for host in args.allowed:
        for operation in ("A:", "lookup:", "ADDRCONFIG:"):
            result = by_name[operation + host]
            assert result["ok"] and result["value"], result
    assert not by_name["denied:" + args.denied]["ok"], "undeclared DNS name was permitted"
    if args.tcp_port:
        assert by_name["TCP:" + args.allowed[0]]["ok"], by_name["TCP:" + args.allowed[0]]
    if args.loopback_port or args.loopback_controls:
        assert not by_name["loopback-TCP"]["ok"], "family discovery fix permitted host loopback TCP"
    if args.loopback_controls:
        assert not by_name["loopback-UDP-payload"]["ok"], "family discovery fix permitted host loopback UDP payload"
    print("PASS: declared DNS including AI_ADDRCONFIG; undeclared name denied")


if __name__ == "__main__":
    main()
