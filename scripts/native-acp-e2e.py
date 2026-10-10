#!/usr/bin/env python3
"""Codeg HTTP -> ConnectionManager -> installed ACP -> native CLI -> fake model.

No cargo invocation, account/config reuse, direct ACP requests, or transcript edits.
Requires Python 3.10+ and websocket-client. All generated files live below --output.
Exit 0 = tested checks passed (inspect not_tested), 1 = failure, 2 = blocked.
Example: python scripts/native-acp-e2e.py --server target/debug/codeg-server.exe
         --output C:/path/to/audit
"""
import argparse
import copy
import ctypes
import gzip
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import traceback
import urllib.error
import urllib.request
import uuid


def require(value, message):
    if not value:
        raise AssertionError(message)
    return value


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def text_of(blocks):
    if isinstance(blocks, str):
        return blocks
    return "".join(b.get("text", "") for b in (blocks or []) if b.get("type") in ("text", "input_text", "output_text"))


def unwrap(result):
    require(not isinstance(result, dict) or result.get("status", "ok") in ("ok", "partial"), result)
    if isinstance(result, dict):
        return result.get("data", result.get("result", result))
    return result


class OwnedJob:
    """Windows kill-on-close job contains even detached/orphaned test descendants."""
    def __init__(self, process):
        from ctypes import wintypes as w
        class Basic(ctypes.Structure):
            _fields_ = [("per_process", ctypes.c_longlong), ("per_job", ctypes.c_longlong),
                        ("flags", w.DWORD), ("min_working", ctypes.c_size_t), ("max_working", ctypes.c_size_t),
                        ("active_limit", w.DWORD), ("affinity", ctypes.c_size_t), ("priority", w.DWORD), ("scheduling", w.DWORD)]
        class IO(ctypes.Structure):
            _fields_ = [(x, ctypes.c_ulonglong) for x in ("read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes")]
        class Extended(ctypes.Structure):
            _fields_ = [("basic", Basic), ("io", IO), ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
                        ("peak_process_memory", ctypes.c_size_t), ("peak_job_memory", ctypes.c_size_t)]
        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        self.api.CreateJobObjectW.argtypes, self.api.CreateJobObjectW.restype = [ctypes.c_void_p, w.LPCWSTR], w.HANDLE
        self.api.SetInformationJobObject.argtypes = [w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD]
        self.api.AssignProcessToJobObject.argtypes = [w.HANDLE, w.HANDLE]
        self.api.CloseHandle.argtypes = [w.HANDLE]
        self.handle = self.api.CreateJobObjectW(None, None)
        require(self.handle, "CreateJobObject failed")
        info = Extended()
        info.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, no breakaway.
        try:
            require(self.api.SetInformationJobObject(self.handle, 9, ctypes.byref(info), ctypes.sizeof(info)), "job limits failed")
            require(self.api.AssignProcessToJobObject(self.handle, int(process._handle)), "job assignment failed")
        except Exception:
            self.close()
            raise

    def close(self):
        if self.handle:
            require(self.api.CloseHandle(self.handle), "job close failed")
            self.handle = None


class Gate:
    def __init__(self, marker, hold=False, tool=False, action=None):
        self.marker, self.tool = marker, tool
        self.action = action
        self.seen, self.release, self.done = threading.Event(), threading.Event(), threading.Event()
        self.answer = "REPLY_" + marker
        if not hold:
            self.release.set()


class Provider:
    """Strict scripted model; unarmed requests fail, and proxy egress is denied."""
    def __init__(self, timeout):
        self.timeout = timeout
        self.requests, self.auxiliary, self.denied, self.errors, self.gates = [], [], [], [], []
        self.pending = queue.Queue()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def reply(self, status, body, mime="application/json"):
                raw = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", mime)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                try:
                    self.wfile.write(raw)
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass

            def deny(self):
                owner.denied.append({"method": self.command, "path": self.path})
                self.reply(403, {"error": "isolated native e2e denies external traffic"})

            do_CONNECT = do_GET = deny

            def do_POST(self):
                gate = None
                try:
                    if not self.path.startswith("/"):
                        return self.deny()
                    raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    if self.headers.get("Content-Encoding") == "gzip":
                        raw = gzip.decompress(raw)
                    body = json.loads(raw or b"{}")
                    for key in ("Authorization", "x-api-key"):
                        require(self.headers.get(key) in (None, "native-e2e-dummy-key", "Bearer native-e2e-dummy-key"), "non-fixture credential received")
                    path = self.path.split("?")[0]
                    if path.endswith("/messages/count_tokens"):
                        owner.auxiliary.append({"path": path, "body": body})
                        return self.reply(200, {"input_tokens": 30})
                    codex = path == "/v1/responses"
                    if not codex and path not in ("/v1/messages", "/messages"):
                        return self.deny()
                    schema = body.get("text", {}).get("format", {}).get("schema", {})
                    title = codex and schema.get("required") == ["title"] and "generate a very short title" in json.dumps(body)
                    if not codex:
                        schema = body.get("output_config", {}).get("format", {}).get("schema", {})
                        title = (schema.get("required") == ["title"] and schema.get("properties") == {"title": {"type": "string"}}
                                 and "You are naming a coding session" in text_of(body.get("system")))
                    record = {"time": time.time(), "path": path, "body": body}
                    messages = body.get("messages", [])
                    if (not codex and body.get("stream") is not True and body.get("max_tokens") == 1
                            and len(messages) == 1 and messages[0].get("role") == "user"
                            and text_of(messages[0].get("content")) == "Hi"
                            and "You are Claude Code, Anthropic's official CLI for Claude." in text_of(body.get("system"))):
                        owner.auxiliary.append({**record, "classification": "native_one_token_api_probe"})
                        return self.reply(200, {"id": "probe_" + uuid.uuid4().hex, "type": "message", "role": "assistant",
                                               "model": body["model"], "content": [{"type": "text", "text": "Hi"}],
                                               "stop_reason": "max_tokens", "stop_sequence": None,
                                               "usage": {"input_tokens": 1, "output_tokens": 1}})
                    if title:
                        owner.auxiliary.append(record)
                        answer = '{"title":"Isolated HTTP native test"}'
                    else:
                        owner.requests.append(record)
                        gate = owner.pending.get_nowait()
                        require(gate.marker in json.dumps(body, ensure_ascii=False), "model context missing expected marker " + gate.marker)
                        gate.seen.set()
                        require(gate.release.wait(owner.timeout), "held model request not released")
                        answer = gate.answer
                    ident = uuid.uuid4().hex
                    if codex:
                        item = {"type": "message", "role": "assistant", "id": "msg-" + ident,
                                "content": [{"type": "output_text", "text": answer}]}
                        if gate and gate.tool:
                            tools = body.get("tools", [])
                            candidates = []
                            for t in tools:
                                if t.get("type") == "namespace":
                                    candidates.extend((x, t.get("name")) for x in t.get("tools", []))
                                else:
                                    candidates.append((t, None))
                            tool, namespace = next((t, n) for t, n in candidates if t.get("name") in ("shell_command", "exec_command"))
                            args = {"command": "echo NATIVE_E2E_APPROVAL", "timeout_ms": 1000} if tool["name"] == "shell_command" else {"cmd": "echo NATIVE_E2E_APPROVAL", "max_output_tokens": 100}
                            args.update(sandbox_permissions="require_escalated", justification="isolated e2e approval guard")
                            item = {"type": "function_call", "id": "fc-" + ident, "call_id": "call-" + ident,
                                    "name": tool["name"], "arguments": json.dumps(args)}
                            if namespace:
                                item["namespace"] = namespace
                        if gate and gate.action:
                            action = gate.action
                            candidates = []
                            for t in body.get("tools", []):
                                candidates.extend((x, t.get("name")) for x in t.get("tools", [])) if t.get("type") == "namespace" else candidates.append((t, None))
                            if action["type"] == "shell":
                                tool, namespace = next((t, n) for t, n in candidates if t.get("name") in ("shell_command", "exec_command"))
                                args = ({"command": action["command"], "timeout_ms": 10000} if tool["name"] == "shell_command"
                                        else {"cmd": action["command"], "max_output_tokens": 200})
                                item = {"type": "function_call", "id": "fc-" + ident, "call_id": action["id"],
                                        "name": tool["name"], "arguments": json.dumps(args)}
                            else:
                                require(action["type"] == "patch", "unexpected Codex fixture action")
                                tool, namespace = next((t, n) for t, n in candidates if t.get("name") == "apply_patch")
                                require(tool.get("type") == "custom", "native apply_patch custom tool missing")
                                item = {"type": "custom_tool_call", "id": "fc-" + ident, "call_id": action["id"],
                                        "name": "apply_patch", "input": action["patch"]}
                            if namespace:
                                item["namespace"] = namespace
                        streaming = []
                        if item["type"] == "message":
                            streaming = [
                                {"type": "response.output_item.added", "output_index": 0, "item": {**item, "content": [], "status": "in_progress"}},
                                {"type": "response.content_part.added", "item_id": item["id"], "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}},
                                {"type": "response.output_text.delta", "item_id": item["id"], "output_index": 0, "content_index": 0, "delta": answer},
                                {"type": "response.output_text.done", "item_id": item["id"], "output_index": 0, "content_index": 0, "text": answer}]
                        elif item["type"] == "custom_tool_call":
                            streaming = [
                                {"type": "response.output_item.added", "output_index": 0, "item": {**item, "input": "", "status": "in_progress"}},
                                {"type": "response.custom_tool_call_input.delta", "item_id": item["id"], "call_id": item["call_id"], "delta": item["input"]}]
                        events = [{"type": "response.created", "response": {"id": "resp-" + ident}}, *streaming,
                                  {"type": "response.output_item.done", "output_index": 0, "item": item},
                                  {"type": "response.completed", "response": {"id": "resp-" + ident,
                                   "usage": {"input_tokens": 30, "output_tokens": 10, "total_tokens": 40}}}]
                        data = "".join("data: " + json.dumps(e) + "\n\n" for e in events).encode()
                    else:
                        block = {"type": "text", "text": answer}
                        if gate and gate.action:
                            action = gate.action
                            require(any(t.get("name") == action["name"] for t in body.get("tools", [])), "native Claude file tool not advertised")
                            block = {"type": "tool_use", "id": action["id"], "name": action["name"], "input": action["input"]}
                        events = [("message_start", {"message": {"id": "msg_" + ident, "type": "message", "role": "assistant",
                                   "model": body.get("model"), "content": [], "stop_reason": None, "stop_sequence": None,
                                   "usage": {"input_tokens": 30, "output_tokens": 0}}}),
                                  ("content_block_start", {"index": 0, "content_block": {"type": "text", "text": ""} if block["type"] == "text" else {**block, "input": {}}}),
                                  ("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": answer} if block["type"] == "text" else {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}}),
                                  ("content_block_stop", {"index": 0}),
                                  ("message_delta", {"delta": {"stop_reason": "end_turn" if block["type"] == "text" else "tool_use", "stop_sequence": None}, "usage": {"output_tokens": 10}}),
                                  ("message_stop", {})]
                        data = "".join("event: " + k + "\ndata: " + json.dumps({"type": k, **v}) + "\n\n" for k, v in events).encode()
                    self.reply(200, data, "text/event-stream")
                except Exception as e:
                    owner.errors.append(repr(e))
                    self.reply(500, {"error": "local model fixture failure"})
                finally:
                    if gate:
                        gate.done.set()

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.base = "http://127.0.0.1:" + str(self.server.server_port)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def arm(self, marker, hold=False, tool=False, action=None):
        gate = Gate(marker, hold, tool, action)
        self.gates.append(gate)
        self.pending.put(gate)
        return gate

    def close(self):
        for gate in self.gates:
            gate.release.set()
        self.server.shutdown()
        self.server.server_close()


class Audit:
    def __init__(self, args):
        self.args = args
        self.root = Path(args.output).resolve() / (time.strftime("run-%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:8])
        self.root.mkdir(parents=True)
        self.report = {"schemaVersion": 1, "success": False, "status": "running", "run": str(self.root),
                       "checks": [], "not_tested": [], "failures": [], "scope": "Codeg HTTP through ConnectionManager, real installed ACP and native binaries, loopback models"}
        self.trace, self.events, self.processes = [], [], []
        self.model, self.ws, self.server = None, None, None
        self.job = None
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.token = "e2e-" + uuid.uuid4().hex
        self.connections = []
        self.links, self.session_links = {}, {}

    def check(self, name, **details):
        self.report["checks"].append({"name": name, "scenario": getattr(self, "scenario", "setup"), "status": "passed", **details})
        print("PASS " + name, flush=True)
        self.flush()

    def untested(self, name, reason):
        self.report["not_tested"].append({"name": name, "reason": reason})

    def flush(self):
        planned = {
            "codex_whitespace_historical_rewind": "codex.historical_rewind_same_id_no_auto_send",
            "codex_whitespace_first_rewind": "codex.first_rewind_raw_whitespace_same_id",
            "codex_plain_history_first_resume": "codex.plain_cold_resume_new_history_no_fork",
            "claude_context_summary": "claude_code.context_summary",
            "claude_context_full": "claude_code.context_full",
            "claude_historical_rewind": "claude_code.historical_rewind_same_id_no_auto_send",
            "claude_first_rewind": "claude_code.first_rewind_raw_whitespace_same_id",
            "codex_queue_add_edit_delete_reorder": "codex.queue_add_edit_delete_reorder_and_scope",
            "codex_queue_cancel": "codex.queue_cancel_preserves_pending_no_delayed_dispatch",
            "codex_queue_start_and_lifecycle": "codex.queue_explicit_nonhead_start_drain_history_lifecycle",
            "codex_real_approval_isolation": "codex.queue_stale_approval_cannot_resolve_next_turn",
            "codex_http_file_restore": "codex.files_apply_exact_bytes_history_preserved",
            "codex_http_file_conflict": "codex.files_stale_preview_conflict_nonmutating",
            "claude_http_file_restore": "claude_code.files_apply_exact_bytes_history_preserved",
            "claude_http_skipped_links": "claude_code.files_hardlink_skipped_regular_restored",
        }
        passed = {c["name"] for c in self.report["checks"]}
        self.report["coverage"] = {key: {"status": "passed" if check in passed else "not_verified", "requiredCheck": check}
                                   for key, check in planned.items()}
        save(self.root / "report.json", self.report)
        save(self.root / "http.json", self.trace)
        save(self.root / "events.json", self.events)
        if self.model:
            save(self.root / "model.json", {"requests": self.model.requests, "auxiliary": self.model.auxiliary,
                 "denied": self.model.denied, "errors": self.model.errors})

    def call(self, method, params=None, refuse=False):
        req = urllib.request.Request(self.base + "/api/" + method,
              data=json.dumps(params or {}).encode(), headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"})
        try:
            with self.http.open(req, timeout=self.args.timeout) as res:
                status, raw = res.status, res.read().decode()
        except urllib.error.HTTPError as e:
            status, raw = e.code, e.read().decode()
        try:
            value = json.loads(raw)
        except ValueError:
            value = raw
        self.trace.append({"time": time.time(), "method": method, "params": params or {}, "status": status, "response": value})
        if refuse:
            require(status >= 400, {"expected": "rejection", "method": method, "response": value})
        else:
            require(200 <= status < 300, {"method": method, "status": status, "response": value})
        return value

    def wait(self, predicate, label, timeout=None):
        end = time.monotonic() + (timeout or self.args.timeout)
        last = None
        while time.monotonic() < end:
            if self.server:
                require(self.server.poll() is None, "server exited while waiting: " + label)
            require(not self.model or not self.model.errors, self.model.errors if self.model else "")
            last = predicate()
            if last:
                return last
            time.sleep(0.1)
        raise TimeoutError(label + ": " + repr(last))

    def setup(self):
        server = Path(self.args.server).resolve() if self.args.server else Path(__file__).resolve().parents[1] / "src-tauri/target/debug/codeg-server.exe"
        if not server.is_file():
            self.report.update(status="blocked", blockedReason="server binary unavailable; main thread must build --no-default-features --features server-bin --bin codeg-server; this script never invokes cargo")
            self.untested("all_HTTP_scenarios", "missing server: " + str(server))
            return False
        import websocket
        node = Path(shutil.which("node") or "missing-node").resolve()
        require(node.is_file(), "node not found")
        npm_root = Path(self.args.npm_root).resolve() if self.args.npm_root else node.parent / "node_modules"
        codex = npm_root / "@agentclientprotocol/codex-acp"
        claude = npm_root / "@agentclientprotocol/claude-agent-acp"
        native = next(iter((codex / "node_modules/@openai").rglob("codex.exe")), None)
        if self.args.codex_native:
            native = Path(self.args.codex_native).resolve()
        require(native and native.is_file(), "installed native Codex executable not found; use --codex-native")
        artifacts = {"server": {"path": str(server), "sha256": sha(server)}, "node": {"path": str(node), "sha256": sha(node)},
                     "codex_native": {"path": str(native), "sha256": sha(native)}}
        runtime = self.root / "runtime"
        runtime.mkdir()
        runtime_server = runtime / server.name
        shutil.copy2(server, runtime_server)
        require(sha(runtime_server) == artifacts["server"]["sha256"], "server changed while copying")
        artifacts["server"]["runtimePath"] = str(runtime_server)
        sidecars = Path(self.args.sidecar_dir).resolve() if self.args.sidecar_dir else Path(__file__).resolve().parents[1] / "src-tauri/target/x86_64-pc-windows-msvc/release"
        for name in ("codeg-mcp.exe", "codeg-computer-helper.exe"):
            source = sidecars / name
            require(source.is_file() and source.stat().st_size > 0, "real sidecar required: " + str(source))
            destination = runtime / name
            shutil.copy2(source, destination)
            artifacts[name] = {"path": str(source), "runtimePath": str(destination), "sha256": sha(destination)}
            require(artifacts[name]["sha256"] == sha(source), "sidecar changed while copying")
        self.model = Provider(self.args.timeout)
        home, data, work, tmp, bins = (self.root / x for x in ("home", "data", "workspace", "tmp", "bin"))
        for p in (home / ".codex", home / ".claude", home / "AppData/Roaming", home / "AppData/Local", home / ".config", home / ".cache", data, work, tmp, bins, self.root / "static"):
            p.mkdir(parents=True, exist_ok=True)
        # Transparent stdout observer, not an ACP emulator. Requests still originate in Codeg.
        observer = self.root / "observe.cjs"
        observer.write_text(r"""
const fs = require('node:fs');
const cp = require('node:child_process');
const {StringDecoder} = require('node:string_decoder');
const log = value => {
  try { fs.appendFileSync(process.env.E2E_OBSERVE,
    JSON.stringify({pid:process.pid,time:Date.now(),...value})+'\n'); } catch {}
};
log({kind:'process',argv:process.argv});
function lines(kind) {
  let buffer = ''; const decoder = new StringDecoder('utf8');
  return chunk => {
    buffer += Buffer.isBuffer(chunk) ? decoder.write(chunk) : String(chunk);
    let n;
    while ((n=buffer.indexOf('\n')) >= 0) {
      const line=buffer.slice(0,n); buffer=buffer.slice(n+1);
      try { log({kind,message:JSON.parse(line)}); } catch {}
    }
  };
}
const observeOut=lines('stdout'), observeIn=lines('stdin');
const write=process.stdout.write;
process.stdout.write=function(chunk,...args) {
  observeOut(chunk); return write.call(this,chunk,...args);
};
// Observe emission without adding a data listener or forcing flowing mode.
const emit=process.stdin.emit;
process.stdin.emit=function(event,...args) {
  if(event==='data') observeIn(args[0]);
  return emit.call(this,event,...args);
};
const spawn=cp.spawn;
cp.spawn=function(...args) {
  const child=spawn.apply(this,args);
  log({kind:'spawn',childPid:child.pid,command:args[0]});
  return child;
};
""", encoding="utf-8")
        for name, package in (("codex-acp", codex), ("claude-agent-acp", claude)):
            manifest = json.loads((package / "package.json").read_text(encoding="utf-8"))
            entry_rel = manifest["bin"] if isinstance(manifest["bin"], str) else manifest["bin"][name]
            entry = package / entry_rel
            require(entry.is_file(), str(entry))
            artifacts[name] = {"path": str(entry), "sha256": sha(entry), "version": manifest["version"]}
            if name == "claude-agent-acp":
                artifacts[name]["runtimeHashes"] = {p.name: sha(p) for p in (package / "dist").glob("*.js")}
            require(os.name == "nt", "currently supports Windows installed ACP launchers")
            (bins / (name + ".cmd")).write_text('@echo off\r\n"' + str(node) + '" "' + str(entry) + '" %*\r\n', encoding="utf-8")
        env = {k: v for k, v in os.environ.items() if k.upper() in ("SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "SYSTEMDRIVE")}
        paths = [str(bins), str(node.parent), str(Path(os.environ.get("SYSTEMROOT", "C:/Windows")) / "System32")]
        git = shutil.which("git")
        if git:
            paths.append(str(Path(git).parent))
        env.update(PATH=os.pathsep.join(paths), HOME=str(home), USERPROFILE=str(home), APPDATA=str(home / "AppData/Roaming"),
                   LOCALAPPDATA=str(home / "AppData/Local"), XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"),
                   XDG_CACHE_HOME=str(home / ".cache"), TEMP=str(tmp), TMP=str(tmp), TMPDIR=str(tmp),
                   CODEX_HOME=str(home / ".codex"), CLAUDE_CONFIG_DIR=str(home / ".claude"), CLAUDE_SECURESTORAGE_CONFIG_DIR=str(home / ".claude"),
                   CODEG_HOME=str(data), CODEG_DATA_DIR=str(data), CODEG_STATIC_DIR=str(self.root / "static"), CODEG_TOKEN=self.token,
                   CODEG_HOST="127.0.0.1", CODEG_ACP_DEBUG="1", CODEG_ACP_HOST_TOOLS="agent",
                   CODEG_LOG="info,codeg_lib::acp::workspace_history=debug",
                   CODEX_PATH=str(native), MODEL_PROVIDER="mock", NO_BROWSER="1", ANTHROPIC_API_KEY="native-e2e-dummy-key",
                   ANTHROPIC_BASE_URL=self.model.base, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", CLAUDE_CODE_DISABLE_BACKGROUND_TASKS="1",
                   CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION="false", NODE_OPTIONS="--require=" + json.dumps(str(observer)),
                   E2E_OBSERVE=str(self.root / "raw-acp.jsonl"))
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            env[key] = self.model.base
        env.update(NO_PROXY="127.0.0.1,localhost,::1", no_proxy="127.0.0.1,localhost,::1")
        if any(name in self.args.scenarios.split(",") for name in ("files-codex", "files-claude")):
            (home / "empty-git-config").write_bytes(b"")
            env.update(GIT_CONFIG_GLOBAL=str(home / "empty-git-config"), GIT_CONFIG_NOSYSTEM="1", GIT_OPTIONAL_LOCKS="0")
        provider = {"name": "E2E loopback", "base_url": self.model.base + "/v1", "wire_api": "responses", "requires_openai_auth": False,
                    "request_max_retries": 0, "stream_max_retries": 0}
        config = {"model": "gpt-5.5", "model_provider": "mock", "model_providers": {"mock": provider}, "approval_policy": "on-request",
                  "sandbox_mode": "workspace-write", "features": {"shell_snapshot": False, "remote_models": False, "plugins": False, "code_mode": False}}
        env["CODEX_CONFIG"] = json.dumps(config)
        (home / ".codex/config.toml").write_text('model="gpt-5.5"\nmodel_provider="mock"\napproval_policy="on-request"\nsandbox_mode="workspace-write"\n[features]\nshell_snapshot=false\nremote_models=false\nplugins=false\ncode_mode=false\n[model_providers.mock]\n' + "\n".join(k + "=" + json.dumps(v) for k, v in provider.items()) + "\n", encoding="utf-8")
        save(home / ".claude/settings.json", {"disableAllHooks": True, "autoMemoryEnabled": False, "model": "claude-sonnet-4-5-20250929"})
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env["CODEG_PORT"] = str(port)
        self.base = "http://127.0.0.1:" + str(port)
        self.report.update(artifacts=artifacts, endpoint=self.base, environment={k: v for k, v in env.items() if k not in ("CODEG_TOKEN", "ANTHROPIC_API_KEY")})
        self.log = (self.root / "server.log").open("wb")
        self.server = subprocess.Popen([str(runtime_server)], cwd=work, env=env, stdout=self.log, stderr=subprocess.STDOUT,
                                       creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0), start_new_session=os.name != "nt")
        if os.name == "nt":
            self.job = OwnedJob(self.server)
        self.report["ownedServerPid"] = self.server.pid
        def ready():
            try:
                self.call("acp_list_connections")
                return True
            except (OSError, urllib.error.URLError):
                return False
        self.wait(ready, "Codeg server readiness")
        self.ws = websocket.create_connection(self.base.replace("http:", "ws:") + "/ws/events", timeout=2,
                  header=["Authorization: Bearer " + self.token], subprotocols=["codeg-events"], http_proxy_host=None)
        self.events.append(json.loads(self.ws.recv()))
        self.ws_open = True
        def reader():
            while self.ws_open:
                try:
                    raw = self.ws.recv()
                    if raw:
                        self.events.append(json.loads(raw))
                except websocket.WebSocketTimeoutException:
                    continue
                except Exception:
                    break
        threading.Thread(target=reader, daemon=True).start()
        self.work = work
        self.child_env = env
        self.check("isolated_server_started", pid=self.server.pid)
        return True

    def snap(self, conn):
        return self.call("acp_get_session_snapshot", {"connectionId": conn})

    def native(self, conn, op, params, refuse=False):
        return self.call("acp_native_operation", {"connectionId": conn, "operation": op, "params": params}, refuse)

    def connect(self, agent, session=None, work=None, mode=None):
        work = work or self.work / agent
        work.mkdir(exist_ok=True)
        folder = self.call("open_folder", {"path": str(work)})
        conn = self.call("acp_connect", {"agentType": agent, "workingDir": str(work), "sessionId": session,
                                        **({"preferredModeId": mode} if mode else {})})
        self.connections.append(conn)
        self.links[conn] = {"folderId": folder["id"]}
        if session in self.session_links:
            self.links[conn].update(self.session_links[session])
        snap = self.wait(lambda: (s if (s := self.snap(conn)) and s.get("external_id") and s.get("status") == "connected" else None), "ACP connect " + agent)
        self.ws.send(json.dumps({"action": "attach", "subscription_id": conn, "connection_id": conn}))
        self.wait(lambda: any(e.get("type") == "snapshot" and e.get("connection_id") == conn for e in self.events), "WS attach snapshot")
        caps = self.call("acp_native_capabilities", {"connectionId": conn})
        save(self.root / (agent + "-capabilities.json"), caps)
        return conn, snap["external_id"]

    def history(self, agent, sid):
        return self.call("get_conversation", {"agentType": agent, "conversationId": sid})

    def prompt(self, conn, value):
        result = self.call("acp_prompt", {"connectionId": conn, "blocks": [{"type": "text", "text": value}],
                          "clientMessageId": str(uuid.uuid4()), **self.links[conn]})
        snap = self.snap(conn)
        if snap and snap.get("conversation_id"):
            self.links[conn]["conversationId"] = snap["conversation_id"]
            self.session_links[snap["external_id"]] = dict(self.links[conn])
        return result

    def idle(self, conn):
        return self.wait(lambda: (s if (s := self.snap(conn)) and s.get("status") == "connected" else None), "turn idle")

    def send(self, conn, marker, value=None):
        offset = len(self.events)
        gate = self.model.arm(marker)
        self.prompt(conn, value or marker)
        self.wait(gate.seen.is_set, "provider saw " + marker)
        self.wait(lambda: any(e.get("type") == "turn_complete" for e in self.conn_events(conn, offset)), "Codeg TurnComplete " + marker)
        self.idle(conn)
        return gate

    def conn_events(self, conn, offset=0):
        return [e["envelope"] for e in self.events[offset:] if e.get("type") == "event" and e.get("envelope", {}).get("connection_id") == conn]

    def raw_messages(self, direction=None):
        path = self.root / "raw-acp.jsonl"
        result = []
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                try:
                    row = json.loads(line)
                    if direction is None or row.get("kind") == direction:
                        result.append(row)
                except ValueError:
                    pass  # A final partial line may be concurrently in flight.
        return result

    def raw_points(self, sid, agent):
        if agent == "claude_code":
            base = self.root / "home/.claude/projects"
            if os.name == "nt":
                base = Path("\\\\?\\" + str(base))
            points = []
            for path in base.rglob(sid + ".jsonl"):
                for line in path.read_text(encoding="utf-8").splitlines():
                    row = json.loads(line)
                    content = row.get("message", {}).get("content")
                    if row.get("type") == "user" and row.get("uuid") and content is not None:
                        raw = text_of(content)
                        if raw and not any(b.get("type") == "tool_result" for b in (content if isinstance(content, list) else [])):
                            points.append({"messageId": row["uuid"], "text": raw,
                                           "messageFingerprint": "sha256:" + hashlib.sha256(raw.encode()).hexdigest(),
                                           "source": "owned Claude native JSONL human user"})
            return points
        grouped = {}
        for row in self.raw_messages("stdout"):
            message = row.get("message", {})
            params = message.get("params", {})
            update = params.get("update", {})
            if message.get("method") != "session/update" or params.get("sessionId") != sid:
                continue
            if update.get("sessionUpdate") != "user_message_chunk" or not update.get("messageId"):
                continue
            key = update["messageId"]
            grouped[key] = grouped.get(key, "") + text_of([update.get("content", {})])
        return [{"messageId": mid, "text": text, "messageFingerprint": "sha256:" + hashlib.sha256(text.encode()).hexdigest()}
                for mid, text in grouped.items()]

    def verify_rewind_wire(self, sid, marker, points):
        target = next((p for p in points if marker in p["text"]), None)
        if target is None:
            # Codex live/resume does not echo human input; compare the exact
            # Codeg-originated ACP prompt bytes, also present in native history.
            originals = [r["message"]["params"] for r in self.raw_messages("stdin")
                         if r.get("message", {}).get("method") == "session/prompt"
                         and r["message"].get("params", {}).get("sessionId") == sid]
            texts = [text_of(p.get("prompt")) for p in originals if marker in text_of(p.get("prompt"))]
            require(len(texts) == 1, "raw prompt boundary ambiguous or absent")
            target = {"text": texts[0], "messageFingerprint": "sha256:" + hashlib.sha256(texts[0].encode()).hexdigest(),
                      "source": "observed Codeg ACP session/prompt; ID may use verified unique-text fallback"}
            native_matches = []
            for path in (self.root / "home/.codex/sessions").rglob("*" + sid + "*.jsonl"):
                try:
                    lines = path.read_text(encoding="utf-8").splitlines()
                except OSError:
                    continue
                for line in lines:
                    try:
                        row = json.loads(line)
                    except ValueError:
                        continue
                    payload = row.get("payload", {})
                    if row.get("type") == "response_item" and payload.get("role") == "user":
                        native_text = text_of(payload.get("content"))
                        if native_text == texts[0]:
                            native_matches.append({"path": str(path.relative_to(self.root)), "messageId": payload.get("id"),
                                                   "sha256": hashlib.sha256(native_text.encode()).hexdigest()})
            require(native_matches, "exact raw prompt missing from owned native rollout")
            target["nativeRolloutEvidence"] = native_matches
        requests = [r["message"] for r in self.raw_messages("stdin") if r.get("message", {}).get("method") == "_session/rewind"
                    and r["message"].get("params", {}).get("sessionId") == sid]
        wire = require(requests, "no Codeg-originated rewind observed")[-1]["params"]
        point = wire["beforeMessage"]
        require(("messageId" not in target or point["messageId"] == target["messageId"]) and point["messageFingerprint"] == target["messageFingerprint"],
                {"wire": wire, "raw": target})
        require(not any("fork" in r.get("message", {}).get("method", "").lower() for r in self.raw_messages("stdin")), "unexpected native fork RPC")
        return {"wire": wire, "raw": target}

    @staticmethod
    def expected(turn):
        return {"turnId": turn["id"], "expectedTurn": {"timestamp": turn.get("timestamp"),
                "agentMessageId": turn.get("agent_message_id"), "blocks": copy.deepcopy(turn["blocks"])}}

    def authored(self, history):
        return [(t["role"], text_of(t.get("blocks"))) for t in history.get("turns", []) if t.get("role") in ("user", "assistant")]

    def no_generation(self, count, seconds=0.8):
        time.sleep(seconds)
        require(len(self.model.requests) == count, "operation unexpectedly generated a model request")

    def rewind_scenario(self, agent, replay=False):
        conn, sid = self.connect(agent)
        pfx = agent.upper()
        first, second, edited = (pfx + x for x in ("_KEEP", "_REMOVE", "_EDITED_DRAFT"))
        raw = " \t\n" + first + "\u00a0  \r\n"
        self.send(conn, first, raw)
        self.send(conn, second, "\n\t" + second + "  \n")
        before = self.wait(lambda: (h if len((h := self.history(agent, sid)).get("turns", [])) >= 4 else None), "persisted two turns")
        if replay:
            self.call("acp_disconnect", {"connectionId": conn})
            conn, reloaded_sid = self.connect(agent, sid)
            require(reloaded_sid == sid, "replay changed session identity")
            before = self.history(agent, sid)
        save(self.root / (agent + "-before.json"), before)
        self.check(agent + ".http_two_turns", connectionId=conn, sessionId=sid)
        context = self.native(conn, "runtime_read", {"resource": "context", **({"detail": "summary"} if agent == "claude_code" else {})})
        require(unwrap(context) is not None, context)
        self.check(agent + ".context_summary", result=context)
        if agent == "claude_code":
            full = self.native(conn, "runtime_read", {"resource": "context", "detail": "full"})
            require(unwrap(full) is not None, full)
            self.check(agent + ".context_full", result=full)
        users = [t for t in before["turns"] if t["role"] == "user"]
        require(len(users) == 2, users)
        raw_points = self.raw_points(sid, agent)
        save(self.root / (agent + "-raw-user-points.json"), raw_points)
        params = self.expected(users[1])
        count = len(self.model.requests)
        wrong = copy.deepcopy(params)
        wrong["expectedTurn"]["blocks"] = [{"type": "text", "text": edited}]
        self.native(conn, "rewind", wrong, refuse=True)
        wrong_id = copy.deepcopy(params)
        wrong_id["expectedTurn"]["agentMessageId"] = "forged-native-identity"
        self.native(conn, "rewind", wrong_id, refuse=True)
        self.native(conn, "rewind", {**params, "sessionId": "foreign-session"}, refuse=True)
        self.native(conn, "rewind", {**params, "beforeMessage": {}}, refuse=True)
        assistant = next(t for t in before["turns"] if t["role"] == "assistant")
        self.native(conn, "rewind", self.expected(assistant), refuse=True)
        require(self.authored(self.history(agent, sid)) == self.authored(before), "identity guard rejection changed history")
        self.no_generation(count)
        self.check(agent + ".edited_draft_and_identity_guards")
        # Refresh at point of mutation, matching public UI contract exactly.
        fresh = self.history(agent, sid)
        target = next(t for t in fresh["turns"] if t["role"] == "user" and second in text_of(t["blocks"]))
        ack = unwrap(self.native(conn, "rewind", self.expected(target)))
        require(ack.get("rewound") is True, ack)
        wire = self.verify_rewind_wire(sid, second, raw_points)
        self.no_generation(count)
        after = self.history(agent, sid)
        require(self.authored(after) == self.authored(before)[:2], {"before": self.authored(before), "after": self.authored(after)})
        require(self.snap(conn)["external_id"] == sid, "rewind forked session")
        self.check(agent + ".historical_rewind_same_id_no_auto_send", sessionId=sid, history=self.authored(after), **wire)
        self.send(conn, edited)
        context_after = json.dumps(self.model.requests[-1]["body"], ensure_ascii=False)
        require(first in context_after and edited in context_after and second not in context_after, "edited send retained discarded branch")
        require(self.snap(conn)["external_id"] == sid, "edited send forked session")
        self.check(agent + ".explicit_edited_send_retains_prefix")
        fresh = self.history(agent, sid)
        first_turn = next(t for t in fresh["turns"] if t["role"] == "user")
        count = len(self.model.requests)
        ack = unwrap(self.native(conn, "rewind", self.expected(first_turn)))
        require(ack.get("rewound") is True, ack)
        wire = self.verify_rewind_wire(sid, first, raw_points)
        self.no_generation(count)
        require(self.authored(self.history(agent, sid)) == [], "first rewind must clear visible branch")
        require(self.snap(conn)["external_id"] == sid, "first rewind changed session")
        self.check(agent + ".first_rewind_raw_whitespace_same_id", submitted=raw, **wire)
        self.call("acp_disconnect", {"connectionId": conn})
        resumed, resumed_sid = self.connect(agent, sid)
        require(resumed_sid == sid and self.authored(self.history(agent, sid)) == [], "cold HTTP resume resurrected history")
        self.no_generation(count)
        if agent == "claude_code":
            cold_full = self.native(resumed, "runtime_read", {"resource": "context", "detail": "full"})
            require(unwrap(cold_full) is not None, cold_full)
            self.check(agent + ".cold_context_full", result=cold_full)
        self.send(resumed, pfx + "_AFTER_FIRST")
        new_context = json.dumps(self.model.requests[-1]["body"], ensure_ascii=False)
        require(all(v not in new_context for v in (first, second, edited)), "first rewind leaked discarded context")
        self.check(agent + ".http_resume_and_new_history_same_id", history=self.authored(self.history(agent, sid)))
        if agent == "claude_code":
            held = self.model.arm("CLAUDE_HELD", hold=True)
            self.prompt(resumed, "CLAUDE_HELD")
            self.wait(held.seen.is_set, "Claude held prompt")
            refused = self.call("acp_prompt", {"connectionId": resumed, "blocks": [{"type": "text", "text": "CLAUDE_PENDING"}]}, refuse=True)
            queued = self.native(resumed, "runtime_read", {"resource": "queuedMessages"})
            self.check("claude_code.host_one_prompt_guard", refusal=refused, queued=queued)
            self.untested("claude_code.pending_cancel_real_item", "Codeg HTTP rejects a concurrent prompt; no real pending SDK message can be created through this host route")
            self.call("acp_cancel", {"connectionId": resumed})
            held.release.set()
            self.idle(resumed)
        self.call("acp_disconnect", {"connectionId": resumed})

    def queue_scenario(self):
        conn, sid = self.connect("codex")
        other, other_sid = self.connect("codex")
        held = self.model.arm("QUEUE_ACTIVE", hold=True)
        self.prompt(conn, "QUEUE_ACTIVE")
        self.wait(held.seen.is_set, "held host prompt")
        def q(action, **params):
            return unwrap(self.native(conn, "queue", {"action": action, **params}))
        def listed():
            return q("list")["data"]
        def inp(value):
            return [{"type": "text", "text": value, "text_elements": []}]
        def add(value):
            return q("add", input=inp(value), clientUserMessageId="client-" + value)["queuedSubmission"]
        a, b, deleted = add("QUEUE_A"), add("QUEUE_B"), add("QUEUE_DELETE")
        updated = q("update", queuedSubmissionId=a["id"], input=inp("QUEUE_A_EDITED"))["queuedSubmission"]
        require(updated["clientUserMessageId"] == a["clientUserMessageId"] and updated["input"] == inp("QUEUE_A_EDITED"), updated)
        require(q("delete", queuedSubmissionId=deleted["id"]).get("deleted") is True, "delete false acknowledgement")
        q("reorder", queuedSubmissionIds=[b["id"], a["id"]])
        require([x["id"] for x in listed()] == [b["id"], a["id"]], "reorder mismatch")
        self.call("acp_prompt", {"connectionId": conn, "blocks": inp("HOST_MUST_NOT_OVERLAP")}, refuse=True)
        self.native(conn, "queue", {"action": "list", "sessionId": other_sid}, refuse=True)
        foreign_delete = unwrap(self.native(other, "queue", {"action": "delete", "queuedSubmissionId": a["id"]}))
        require(foreign_delete.get("deleted") is False, foreign_delete)
        require(unwrap(self.native(other, "queue", {"action": "list"}))["data"] == [], "queue leaked to other session")
        self.check("codex.queue_add_edit_delete_reorder_and_scope", pending=listed(), otherSessionId=other_sid)
        count = len(self.model.requests)
        self.call("acp_cancel", {"connectionId": conn})
        held.release.set()
        self.idle(conn)
        preserved = listed()
        self.no_generation(count, 10.5)
        require(listed() == preserved, "cancel changed pending queue")
        self.check("codex.queue_cancel_preserves_pending_no_delayed_dispatch", observationSeconds=10.5)
        marker = len(self.events)
        ga = self.model.arm("QUEUE_A_EDITED", hold=True)
        gb = self.model.arm("QUEUE_B")
        start = q("start", queuedSubmissionId=a["id"])
        self.wait(ga.seen.is_set, "non-head start reaches provider")
        snap = self.snap(conn)
        require(snap["status"] == "prompting", snap)
        for field in ("pending_permission", "pending_question", "pending_plan_approval"):
            require(not snap.get(field) and not self.snap(other).get(field), "approval leaked: " + field)
        self.call("acp_prompt", {"connectionId": conn, "blocks": inp("MUST_NOT_INTERLEAVE")}, refuse=True)
        ga.release.set()
        self.wait(gb.seen.is_set, "remaining queue drains")
        self.wait(lambda: len([e for e in self.conn_events(conn, marker) if e.get("type") == "turn_complete"]) >= 2, "two Codeg queue completions")
        self.idle(conn)
        require(listed() == [], "queue did not drain")
        history = self.history("codex", sid)
        authored = self.authored(history)
        users = [t for role, t in authored if role == "user"]
        require(sum("QUEUE_A_EDITED" in t for t in users) == 1 and sum("QUEUE_B" in t for t in users) == 1, authored)
        require(not any("QUEUE_DELETE" in t or "FOREIGN" in t for t in users), authored)
        require(next(i for i, t in enumerate(users) if "QUEUE_A_EDITED" in t) < next(i for i, t in enumerate(users) if "QUEUE_B" in t), users)
        require(self.snap(conn)["external_id"] == sid, "queue changed native session")
        events = self.conn_events(conn, marker)
        completions = [e for e in events if e.get("type") == "turn_complete"]
        require(len(completions) == 2 and all(e.get("stop_reason") == "end_turn" for e in completions), completions)
        require(len([e for e in events if e.get("type") == "status_changed" and e.get("status") == "prompting"]) == 2, events)
        require(not any(e.get("type") in ("permission_request", "turn_complete", "content_delta") for e in self.conn_events(other, marker)), "native queue events leaked across connections")
        raw_turns = [r["message"]["params"]["turn"] for r in self.raw_messages("stdout") if r.get("message", {}).get("method") == "_session/queue/turn"
                     and r["message"].get("params", {}).get("sessionId") == sid]
        ids = {t["id"] for t in raw_turns}
        require(len(ids) == 2 and all([t["status"] for t in raw_turns if t["id"] == ident] == ["inProgress", "completed"] for ident in ids), raw_turns)
        save(self.root / "queue-events.json", self.events[marker:])
        self.check("codex.queue_explicit_nonhead_start_drain_history_lifecycle", start=start, history=authored, nativeTurns=raw_turns,
                   codegCompletions=completions)
        self.approval_scenario(conn, other, sid)
        for c in (conn, other):
            self.call("acp_disconnect", {"connectionId": c})

    def file_restore_scenario(self, agent):
        work = self.work / (agent + "-files")
        work.mkdir()
        git = require(shutil.which("git"), "Git required for isolated file restore")
        def git_run(*args):
            result = subprocess.run([git, *args], cwd=work, env=self.child_env, capture_output=True,
                                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0), timeout=30)
            require(result.returncode == 0, result.stderr.decode("utf-8", errors="replace"))
            return result.stdout
        git_run("init", "-q")
        git_run("config", "core.autocrlf", "false")
        for name, data in {"changed.txt": b"ORIGINAL\n", "second.txt": b"SECOND\n", "user.txt": b"TRACKED\n"}.items():
            (work / name).write_bytes(data)
        git_run("add", "--", "changed.txt", "second.txt", "user.txt")
        git_run("-c", "user.name=HTTP fixture", "-c", "user.email=fixture@invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "isolated fixture")
        (work / "user.txt").write_bytes(b"UNRELATED STAGED\n")
        git_run("add", "--", "user.txt")
        (work / "user.txt").write_bytes(b"UNRELATED UNSTAGED\n")
        (work / "untracked.txt").write_bytes(b"UNRELATED UNTRACKED\n")
        names = ("changed.txt", "second.txt", "user.txt", "untracked.txt", ".git/index")
        def snapshot():
            return {name: {"sha256": sha(work / name), "bytes": (work / name).read_bytes().hex()} for name in names}
        def paths_match(returned, expected):
            require(isinstance(returned, list), {"missingReturnedPaths": returned})
            actual = {os.path.normcase(str(Path(p).resolve())) for p in returned}
            require(actual == {os.path.normcase(str((work / n).resolve())) for n in expected}, {"actual": returned, "expected": expected})
        baseline = snapshot()
        save(self.root / (agent + "-files-baseline.json"), baseline)
        conn, sid = self.connect(agent, work=work, mode="agent-full-access" if agent == "codex" else "acceptEdits")
        def mutate(marker, files):
            offset, count = len(self.events), len(self.model.requests)
            ids = []
            if agent == "codex":
                call_id = "http-file-patch-" + uuid.uuid4().hex[:12]
                patch = "*** Begin Patch\n" + "".join("*** Update File: " + name + "\n@@\n-" + old + "\n+" + new + "\n" for name, old, new in files) + "*** End Patch\n"
                self.model.arm(marker, action={"type": "patch", "id": call_id, "patch": patch})
                ids.append(call_id)
            else:
                for name, old, new in files:
                    for tool, params in (("Read", {"file_path": str(work / name)}),
                                         ("Edit", {"file_path": str(work / name), "old_string": old, "new_string": new})):
                        call_id = "http_file_" + uuid.uuid4().hex[:12]
                        self.model.arm(marker, action={"name": tool, "input": params, "id": call_id})
                        ids.append(call_id)
            final = self.model.arm(marker)
            self.prompt(conn, marker)
            self.wait(final.seen.is_set, "native file tools reached final model response")
            self.wait(lambda: any(e.get("type") == "turn_complete" for e in self.conn_events(conn, offset)), "file tool Codeg completion")
            self.idle(conn)
            require(len(self.model.requests) == count + len(ids) + 1, "unexpected file model call count")
            final_body = self.model.requests[-1]["body"]
            if agent == "codex":
                outputs = [x for x in final_body.get("input", []) if x.get("type") == "custom_tool_call_output" and x.get("call_id") in ids]
            else:
                outputs = [b for msg in final_body.get("messages", []) for b in msg.get("content", [])
                           if isinstance(b, dict) and b.get("type") == "tool_result" and b.get("tool_use_id") in ids]
                require(not any(x.get("is_error") for x in outputs), outputs)
            require(len(outputs) == len(ids), {"missingNativeToolResults": ids, "outputs": outputs})
            for name, _, new in files:
                require((work / name).read_bytes() == (new + "\n").encode(), "native file bytes mismatch: " + name)
            detail = self.history(agent, sid)
            if agent == "codex":
                parsed_ids = [b.get("tool_use_id") for turn in detail["turns"] for b in turn["blocks"]
                              if b.get("type") == "tool_use" and b.get("tool_name") in ("apply_patch", "fileChange")]
                require(ids[0] in parsed_ids, {"CodegHistoryMissingFileTool": ids[0], "parsed": parsed_ids})
            user = next(t for t in detail["turns"] if t["role"] == "user" and marker in text_of(t["blocks"]))
            self.check(agent + ".files_native_mutation_and_tool_result", marker=marker, toolIds=ids, outputs=outputs, hashes=snapshot())
            return ids[0], user, detail
        tool_id, user, before_history = mutate("HTTP_FILE_RESTORE_MAIN", [("changed.txt", "ORIGINAL", "NATIVE_EDITED")])
        count = len(self.model.requests)
        changed = snapshot()
        require(all(changed[n] == baseline[n] for n in names if n != "changed.txt"), "native tool touched unrelated files/index")
        if agent == "codex":
            op, params = "file_revert", {"toolCallId": tool_id}
        else:
            op, params = "rewind_files", self.expected(user)
        def control(dry, **extras):
            return unwrap(self.native(conn, op, {**params, "dryRun": dry, **extras}))
        def preview():
            result = control(True)
            if agent == "claude_code" and result.get("canRewind") is False and result.get("reason") == "busy":
                return None
            return result
        preview_result = self.wait(preview, "read-only checkpoint preview idle", timeout=8)
        ok_key = "canRevert" if agent == "codex" else "canRewind"
        path_key = "paths" if agent == "codex" else "filesChanged"
        require(preview_result.get(ok_key) is True, preview_result)
        paths_match(preview_result.get(path_key), ["changed.txt"])
        require(snapshot() == changed, "preview changed bytes or index")
        self.check(agent + ".files_preview_paths_nonmutating", preview=preview_result, hashes=snapshot())
        if agent == "codex":
            self.native(conn, op, {**params, "dryRun": False}, refuse=True)
            invalid = control(False, previewToken="sha256:" + "0" * 64)
            require(invalid.get("reason") == "stale_preview" and not invalid.get("reverted") and snapshot() == changed, invalid)
            (work / "changed.txt").write_bytes(b"EXTERNAL_CONFLICT\n")
            conflict_bytes = snapshot()
            stale = control(False, previewToken=preview_result["previewToken"])
            conflict = control(True)
            require(stale.get("reason") == "stale_preview" and not stale.get("reverted"), stale)
            require(conflict.get("reason") == "patch_conflict" and not conflict.get("canRevert"), conflict)
            paths_match(conflict.get("paths"), ["changed.txt"])
            require(snapshot() == conflict_bytes, "conflict check mutated workspace")
            self.check(agent + ".files_stale_preview_conflict_nonmutating", stale=stale, conflict=conflict, hashes=conflict_bytes)
            # Re-establish only the fixture's known native-edited bytes.
            (work / "changed.txt").write_bytes(b"NATIVE_EDITED\n")
            preview_result = control(True)
            require(preview_result.get("canRevert") is True and snapshot() == changed, preview_result)
        restored = control(False, **({"previewToken": preview_result["previewToken"]} if agent == "codex" else {}))
        require(restored.get(ok_key) is True, restored)
        if agent == "codex":
            require(restored.get("reverted") is True, restored)
        # Claude's native apply may only return canRewind/skippedLinks;
        # filesChanged is optional there. Preview paths and exact bytes are mandatory.
        if agent == "codex" or path_key in restored:
            paths_match(restored.get(path_key), ["changed.txt"])
        if agent == "claude_code":
            require(restored.get("sessionId") == sid and restored.get("dryRun") is False
                    and restored.get("skippedLinks", 0) == 0, restored)
        require(snapshot() == baseline, "apply did not exactly restore fixture while preserving unrelated files/index")
        require(self.authored(self.history(agent, sid)) == self.authored(before_history), "file apply changed conversation")
        require(self.snap(conn)["external_id"] == sid and len(self.model.requests) == count, "file restore changed session or generated model traffic")
        self.check(agent + ".files_apply_exact_bytes_history_preserved", result=restored, hashes=snapshot(), sessionId=sid)
        if agent == "claude_code":
            _, user, before_history = mutate("HTTP_FILE_RESTORE_LINK_SKIP", [("changed.txt", "ORIGINAL", "LINK_EDITED"), ("second.txt", "SECOND", "SECOND_EDITED")])
            params = self.expected(user)
            count = len(self.model.requests)
            initial_preview = self.wait(preview, "second checkpoint idle", timeout=8)
            require(initial_preview.get("canRewind") is True, initial_preview)
            paths_match(initial_preview.get("filesChanged"), ["changed.txt", "second.txt"])
            # A second link to this owned file forces native link-safety refusal.
            link = work / "owned-hardlink.txt"
            os.link(work / "changed.txt", link)
            require(os.stat(link).st_nlink >= 2, "hardlink fixture did not link")
            before_skip = snapshot()
            link_hash = sha(link)
            linked_preview = control(True)
            require(linked_preview.get("canRewind") is True and snapshot() == before_skip and sha(link) == link_hash, linked_preview)
            require("skippedLinks" not in linked_preview, "dry-run must not claim native link-safety application")
            skipped = control(False)
            require(skipped.get("canRewind") is True and skipped.get("skippedLinks", 0) >= 1, skipped)
            require((work / "changed.txt").read_bytes() == b"LINK_EDITED\n" and sha(link) == link_hash, "unsafe linked path was overwritten")
            require((work / "second.txt").read_bytes() == b"SECOND\n", "regular path not restored during partial apply")
            require(all(snapshot()[n] == baseline[n] for n in ("user.txt", "untracked.txt", ".git/index")), "partial apply touched unrelated content/index")
            require(self.authored(self.history(agent, sid)) == self.authored(before_history) and len(self.model.requests) == count, "partial file restore changed history or generated")
            self.check(agent + ".files_hardlink_skipped_regular_restored", preview=linked_preview, apply=skipped, hashes=snapshot(), linkedFileSha256=sha(link))
        self.call("acp_disconnect", {"connectionId": conn})

    def workspace_checkpoint_scenario(self, agent):
        """Real shell writes in a non-Git workspace; only owned fixture data changes."""
        work = self.work / (agent + "-workspace-checkpoints")
        work.mkdir()
        (work / "changed.txt").write_bytes(b"ORIGINAL\n")
        (work / "deleted.txt").write_bytes(b"RESTORE_DELETED\n")
        (work / "unrelated.txt").write_bytes(b"KEEP\n")
        require(not (work / ".git").exists(), "fixture must not be a Git checkout")
        conn, sid = self.connect(agent, work=work, mode="agent-full-access" if agent == "codex" else "bypassPermissions")
        caps = self.call("acp_native_capabilities", {"connectionId": conn})
        require("workspaceRewindFiles" in json.dumps(caps), "host checkpoint capability absent")

        def snapshot():
            return {p.name: p.read_bytes().hex() for p in work.iterdir() if p.is_file()}

        def mutate(marker, script):
            offset = len(self.events)
            call_id = "checkpoint_shell_" + uuid.uuid4().hex[:12]
            if agent == "codex":
                action = {"type": "shell", "id": call_id, "command": script}
            else:
                # Claude's Bash launches PowerShell explicitly on Windows.
                import base64
                encoded = base64.b64encode(script.encode("utf-16-le")).decode("ascii")
                powershell = Path(os.environ.get("SYSTEMROOT", "C:/Windows")) / "System32/WindowsPowerShell/v1.0/powershell.exe"
                command = '"' + powershell.as_posix() + '" -NoProfile -NonInteractive -EncodedCommand ' + encoded
                action = {"name": "Bash", "id": call_id, "input": {"command": command, "timeout": 10000}}
            self.model.arm(marker, action=action)
            final = self.model.arm(marker)
            self.prompt(conn, marker)
            self.wait(final.seen.is_set, "shell completed " + marker)
            self.wait(lambda: any(e.get("type") == "turn_complete" for e in self.conn_events(conn, offset)), "checkpoint completion " + marker)
            self.idle(conn)
            detail = self.history(agent, sid)
            target = next(t for t in detail["turns"] if t["role"] == "user" and marker == text_of(t["blocks"]))
            return target, detail

        def control(target, dry=True, **extras):
            return unwrap(self.native(conn, "workspace_rewind_files", {**self.expected(target), "dryRun": dry, **extras}))

        def reject(target, **extras):
            self.native(conn, "workspace_rewind_files", {**self.expected(target), "dryRun": True, **extras}, refuse=True)

        keep, kept_history = mutate("CHECKPOINT_KEEP", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'keep.txt'), 'KEEP_FIRST')")
        require((work / "keep.txt").read_bytes() == b"KEEP_FIRST", "first native shell did not write")
        first_preview = control(keep)
        require(first_preview.get("canRevert") is True and first_preview["paths"] == ["keep.txt"], first_preview)
        self.check(agent + ".workspace_first_prompt_captured")
        baseline = snapshot()
        target, _ = mutate("CHECKPOINT_REMOVE", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'new.txt'), 'NEW_SECOND'); [IO.File]::WriteAllText((Join-Path (Get-Location) 'changed.txt'), 'CHANGED_SECOND'); [IO.File]::Delete((Join-Path (Get-Location) 'deleted.txt'))")
        _, original_history = mutate("CHECKPOINT_LATER", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'new.txt'), 'NEW_THIRD'); [IO.File]::WriteAllText((Join-Path (Get-Location) 'changed.txt'), 'CHANGED_THIRD')")
        require((work / "new.txt").read_bytes() == b"NEW_THIRD" and (work / "changed.txt").read_bytes() == b"CHANGED_THIRD" and not (work / "deleted.txt").exists(), "shell create/edit/delete did not execute")
        changed = snapshot()
        count = len(self.model.requests)
        preview = control(target)
        require(preview.get("canRevert") is True and set(preview["paths"]) == {"new.txt", "changed.txt", "deleted.txt"}, preview)
        require(snapshot() == changed, "preview mutated workspace")
        self.check(agent + ".workspace_shell_span_preview_nonmutating", paths=preview["paths"])

        # A stale preview may never overwrite a later manual edit.
        (work / "changed.txt").write_bytes(b"MANUAL_CONFLICT")
        conflict = snapshot()
        reject(target, dryRun=False, previewToken=preview["previewToken"])
        reject(target)
        require(snapshot() == conflict and self.authored(self.history(agent, sid)) == self.authored(original_history), "conflict refusal mutated files/history")
        self.check(agent + ".workspace_stale_preview_conflict_nonmutating")
        (work / "changed.txt").write_bytes(b"CHANGED_THIRD")

        # Missing historical coverage is refused rather than inferred from current files.
        records = list((self.root / "data/workspace-checkpoints").glob("*/history/*.json"))
        require(records, "no persisted host checkpoint records")
        record = next((p for p in records if len((r := json.loads(p.read_text()))["before_prefix"]) > 0
                       and r["checkpoint"]["before"]["root"].endswith(work.name)), None)
        require(record, "missing subsequent-turn checkpoint")
        saved = record.with_suffix(".fixture-backup")
        record.rename(saved)
        try:
            reject(target)
            require(snapshot() == changed, "missing coverage mutated files")
        finally:
            saved.rename(record)
        self.check(agent + ".workspace_missing_coverage_refused")

        # Close and re-open the native session: snapshots must survive host connections.
        self.call("acp_disconnect", {"connectionId": conn})
        conn, resumed = self.connect(agent, sid, work=work, mode="agent-full-access" if agent == "codex" else "bypassPermissions")
        require(resumed == sid, "cold checkpoint resume forked session")
        preview = control(target)
        # A new unrelated file created outside the recorded turns must survive.
        (work / "outside-span.txt").write_bytes(b"MANUAL_KEEP")
        result = control(target, False, previewToken=preview["previewToken"])
        require(result.get("reverted") is True, result)
        require(snapshot() == {**baseline, "outside-span.txt": b"MANUAL_KEEP".hex()}, "restore did not recover exact pre-turn bytes")
        require(self.authored(self.history(agent, sid)) == self.authored(original_history), "file restore rewound history implicitly")
        reject(target, dryRun=False, previewToken=preview["previewToken"])
        self.call("acp_prompt", {"connectionId": conn, "blocks": [{"type": "text", "text": "MUST_NOT_START_DURING_RESTORE"}], **self.links[conn]}, refuse=True)
        self.native(conn, "rewind", self.expected(keep), refuse=True)
        require(len(self.model.requests) == count, "pending file restore admitted new generation")
        self.check(agent + ".workspace_pending_restore_blocks_new_work_and_wrong_rewind")
        self.call("acp_disconnect", {"connectionId": conn})
        (work / "changed.txt").write_bytes(b"EXTERNAL_AFTER_RESTORE")
        conn, resumed = self.connect(agent, sid, work=work, mode="agent-full-access" if agent == "codex" else "bypassPermissions")
        require(resumed == sid, "file recovery reconnect forked")
        self.native(conn, "rewind", self.expected(target), refuse=True)
        require((work / "changed.txt").read_bytes() == b"EXTERNAL_AFTER_RESTORE"
                and self.authored(self.history(agent, sid)) == self.authored(original_history), "recovery conflict changed files/history")
        self.check(agent + ".workspace_reconnect_revalidates_restored_files")
        # Restore only the fixture's known original bytes, then complete its paired history step.
        (work / "changed.txt").write_bytes(bytes.fromhex(baseline["changed.txt"]))
        ack = unwrap(self.native(conn, "rewind", self.expected(target)))
        require(ack.get("rewound") is True, ack)
        self.wait(lambda: self.authored(self.history(agent, sid)) == self.authored(kept_history), "native history persisted rewind")
        require(self.snap(conn)["external_id"] == sid, "combined restore forked session")
        self.no_generation(count)
        self.check(agent + ".workspace_cold_restore_files_then_history_same_id", sessionId=sid, restored=sorted(result["paths"]), files=snapshot())

        # A manual edit between captured turns breaks ownership of the full span.
        gap_target, _ = mutate("CHECKPOINT_GAP_FIRST", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'gap.txt'), 'FIRST')")
        (work / "gap-manual.txt").write_bytes(b"INTERTURN_MANUAL")
        mutate("CHECKPOINT_GAP_SECOND", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'gap.txt'), 'SECOND')")
        before_gap = snapshot()
        reject(gap_target)
        require(snapshot() == before_gap, "inter-turn gap refusal changed files")
        self.check(agent + ".workspace_interturn_manual_gap_refused")
        self.call("acp_disconnect", {"connectionId": conn})

    def workspace_overlap_scenario(self):
        """A second host writer in a nested workspace invalidates both captures."""
        work = self.work / "overlap-parent"
        child = work / "child"
        child.mkdir(parents=True)
        a, sid_a = self.connect("codex", work=work)
        b, sid_b = self.connect("claude_code", work=child)
        marker_a, marker_b = "OVERLAP_PARENT_ACTIVE", "OVERLAP_CHILD_ACTIVE"
        offset = len(self.events)
        held = self.model.arm(marker_a, hold=True)
        self.prompt(a, marker_a)
        self.wait(held.seen.is_set, "parent model held")
        self.send(b, marker_b)
        held.release.set()
        self.wait(lambda: any(e.get("type") == "turn_complete" for e in self.conn_events(a, offset)), "overlapping parent completion")
        self.idle(a)
        for conn, sid, agent, marker in ((a, sid_a, "codex", marker_a), (b, sid_b, "claude_code", marker_b)):
            turn = next(t for t in self.history(agent, sid)["turns"] if t["role"] == "user" and text_of(t["blocks"]) == marker)
            self.native(conn, "workspace_rewind_files", {**self.expected(turn), "dryRun": True}, refuse=True)
        self.check("workspace.overlapping_parent_child_cross_agent_captures_refused")
        # Isolation protects snapshots without disabling ordinary multi-agent work.
        self.send(a, "AFTER_OVERLAP_QUIET")
        target = next(t for t in self.history("codex", sid_a)["turns"] if t["role"] == "user" and text_of(t["blocks"]) == "AFTER_OVERLAP_QUIET")
        preview = unwrap(self.native(a, "workspace_rewind_files", {**self.expected(target), "dryRun": True}))
        require(preview.get("canRevert") is True and preview.get("paths") == [], preview)
        self.check("workspace.quiet_turn_after_overlap_captures_normally")
        for conn in (a, b):
            self.call("acp_disconnect", {"connectionId": conn})

    def plain_history_scenario(self):
        """Independent baseline exposes later stages even when whitespace fails."""
        conn, sid = self.connect("codex")
        for marker in ("PLAIN_KEEP", "PLAIN_REMOVE"):
            self.send(conn, marker)
        original = self.history("codex", sid)
        for marker, prefix in (("PLAIN_REMOVE", self.authored(original)[:2]), ("PLAIN_KEEP", [])):
            fresh = self.history("codex", sid)
            turn = next(t for t in fresh["turns"] if t["role"] == "user" and text_of(t["blocks"]) == marker)
            count = len(self.model.requests)
            ack = unwrap(self.native(conn, "rewind", self.expected(turn)))
            require(ack.get("rewound") is True, ack)
            self.no_generation(count)
            require(self.authored(self.history("codex", sid)) == prefix, "plain rewind retained wrong prefix")
            require(self.snap(conn)["external_id"] == sid, "plain rewind forked")
            requests = [r["message"] for r in self.raw_messages("stdin") if r.get("message", {}).get("method") == "_session/rewind"
                        and r["message"].get("params", {}).get("sessionId") == sid]
            wire = requests[-1]["params"]
            require(wire["beforeMessage"]["messageFingerprint"] == "sha256:" + hashlib.sha256(marker.encode()).hexdigest(), wire)
            self.check("codex.plain_rewind_" + marker.lower(), sessionId=sid, history=prefix, wire=wire)
            if prefix:
                self.send(conn, "PLAIN_EDITED")
                body = json.dumps(self.model.requests[-1]["body"])
                require("PLAIN_REMOVE" not in body and "PLAIN_KEEP" in body and "PLAIN_EDITED" in body, "plain edited context wrong")
                self.check("codex.plain_explicit_send_after_rewind")
        count = len(self.model.requests)
        self.call("acp_disconnect", {"connectionId": conn})
        conn, new_sid = self.connect("codex", sid)
        require(sid == new_sid and self.authored(self.history("codex", sid)) == [], "cold history resurrected")
        self.no_generation(count)
        self.send(conn, "PLAIN_NEW_AFTER_EMPTY")
        body = json.dumps(self.model.requests[-1]["body"])
        require(all(x not in body for x in ("PLAIN_KEEP", "PLAIN_REMOVE", "PLAIN_EDITED")), "cold resume leaked discarded branch")
        require(not any("fork" in r.get("message", {}).get("method", "").lower() for r in self.raw_messages("stdin")), "unexpected fork RPC")
        self.check("codex.plain_cold_resume_new_history_no_fork", sessionId=sid, history=self.authored(self.history("codex", sid)))
        self.call("acp_disconnect", {"connectionId": conn})

    def approval_scenario(self, conn, other, sid):
        def queue_tool(marker, start=False):
            gate = self.model.arm(marker, tool=True)
            item = unwrap(self.native(conn, "queue", {"action": "add", "input": [{"type": "text", "text": marker}],
                           "clientUserMessageId": "client-" + marker}))["queuedSubmission"]
            if start:
                unwrap(self.native(conn, "queue", {"action": "start", "queuedSubmissionId": item["id"]}))
            self.wait(gate.seen.is_set, "queued native tool model request")
            card = self.wait(lambda: self.snap(conn).get("pending_permission"), "real native permission request")
            require(not self.snap(other).get("pending_permission"), "approval appeared on foreign connection")
            return item, card

        offset = len(self.events)
        item, card = queue_tool("QUEUE_APPROVAL_ONE")
        option = next((x["option_id"] for x in card["options"] if x.get("kind") == "allow_once"), card["options"][0]["option_id"])
        count = len(self.model.requests)
        # This endpoint ACKs receipt of the responder command; inspect state,
        # not its 200 status, to establish an unknown/foreign ID was ignored.
        self.call("acp_respond_permission", {"connectionId": other, "requestId": card["request_id"], "optionId": option})
        self.no_generation(count)
        require(self.snap(conn).get("pending_permission", {}).get("request_id") == card["request_id"], "foreign response consumed owner approval")
        self.call("acp_cancel", {"connectionId": conn})
        self.wait(lambda: any(e.get("type") == "turn_complete" and e.get("stop_reason") == "cancelled" for e in self.conn_events(conn, offset)), "queue cancel completes approval turn")
        self.idle(conn)
        require(not self.snap(conn).get("pending_permission"), "cancel left stale approval")
        unwrap(self.native(conn, "queue", {"action": "list"}))
        self.check("codex.queue_real_approval_foreign_response_and_cancel", requestId=card["request_id"], queuedSubmission=item)
        # A cancelled turn's request ID must not answer the next turn's card.
        offset = len(self.events)
        _, next_card = queue_tool("QUEUE_APPROVAL_TWO", start=True)
        require(next_card["request_id"] != card["request_id"], "approval ID reused")
        count = len(self.model.requests)
        self.call("acp_respond_permission", {"connectionId": conn, "requestId": card["request_id"], "optionId": option})
        self.no_generation(count)
        require(self.snap(conn).get("pending_permission", {}).get("request_id") == next_card["request_id"], "stale response consumed next approval")
        self.call("acp_cancel", {"connectionId": conn})
        self.wait(lambda: any(e.get("type") == "turn_complete" and e.get("stop_reason") == "cancelled" for e in self.conn_events(conn, offset)), "second approval cancellation")
        self.idle(conn)
        require(not self.snap(conn).get("pending_permission"), "second cancellation left approval")
        require(self.snap(conn)["external_id"] == sid, "approval cancellation changed session")
        self.check("codex.queue_stale_approval_cannot_resolve_next_turn", previousRequestId=card["request_id"], currentRequestId=next_card["request_id"])

    def close(self):
        if self.model:
            for gate in self.model.gates:
                gate.release.set()
        self.ws_open = False
        if self.ws:
            self.ws.close()
        if self.job:
            self.job.close()
            self.server.wait(timeout=20)
            self.report["cleanup"] = {"ownedRootPid": self.server.pid, "method": "Windows kill-on-close job",
                                      "jobClosed": True, "rootExitCode": self.server.returncode}
        if self.server and self.server.poll() is None:
            if os.name == "nt":
                cleanup = subprocess.run(["taskkill", "/PID", str(self.server.pid), "/T", "/F"], capture_output=True,
                                         creationflags=subprocess.CREATE_NO_WINDOW, timeout=20)
                self.report["cleanup"] = {"ownedRootPid": self.server.pid, "exitCode": cleanup.returncode,
                                          "stdout": cleanup.stdout.decode(errors="replace"), "stderr": cleanup.stderr.decode(errors="replace")}
            else:
                os.killpg(self.server.pid, signal.SIGTERM)
            self.server.wait(timeout=20)
        if getattr(self, "log", None):
            self.log.close()
        if self.model:
            self.model.close()
        db_path = self.root / "data/codeg.db"
        if db_path.exists():
            db = sqlite3.connect(db_path.as_uri() + "?mode=ro", uri=True)
            try:
                cursor = db.execute("SELECT id, external_id, agent_type, title, status, folder_id FROM conversation ORDER BY id")
                self.report["persistedConversations"] = [dict(zip([x[0] for x in cursor.description], row)) for row in cursor.fetchall()]
            finally:
                db.close()
        self.flush()

    def run(self):
        try:
            if not self.setup():
                return 2
            # Independent scenarios continue, each failure stays visible in the report.
            scenarios = {"codex-live": lambda: self.rewind_scenario("codex"),
                         "codex-replay": lambda: self.rewind_scenario("codex", replay=True),
                         "codex-plain": self.plain_history_scenario,
                         "claude": lambda: self.rewind_scenario("claude_code"), "queue": self.queue_scenario,
                         "files-codex": lambda: self.file_restore_scenario("codex"),
                         "files-claude": lambda: self.file_restore_scenario("claude_code"),
                         "workspace-codex": lambda: self.workspace_checkpoint_scenario("codex"),
                         "workspace-claude": lambda: self.workspace_checkpoint_scenario("claude_code"),
                         "workspace-overlap": self.workspace_overlap_scenario}
            selected = self.args.scenarios.split(",")
            for name in scenarios:
                if name not in selected:
                    self.untested(name, "not selected in --scenarios")
            for name in selected:
                self.scenario = name
                operation = scenarios[name]
                if name == "claude" and self.report["artifacts"]["server"]["sha256"] == "47bcb02dd55c4739ec0164040391614eeba01ab8e857e034dbdbc55c82412b37":
                    self.untested("claude", "known 12:49 server ignores CLAUDE_CONFIG_DIR in config loader; unsafe to rerun isolated Claude until fixed")
                    self.report["failures"].append({"scenario": "claude", "error": "known config isolation defect; preflight blocked"})
                    continue
                try:
                    operation()
                except Exception as e:
                    self.report["failures"].append({"scenario": name, "error": str(e), "traceback": traceback.format_exc()})
                    print("FAIL " + name + ": " + str(e), flush=True)
                    self.flush()
                    # Prevent a failed scenario's held model work contaminating the next.
                    for gate in self.model.gates:
                        gate.release.set()
                    for conn in self.connections:
                        try:
                            self.call("acp_disconnect", {"connectionId": conn})
                        except Exception:
                            pass
                    abandoned_gate = not self.model.pending.empty()
                    while not self.model.pending.empty():
                        self.model.pending.get_nowait()
                    if self.model.errors or abandoned_gate:
                        for remaining in selected[selected.index(name) + 1:]:
                            self.untested(remaining, "fixture failed or had an unconsumed gate; rerun independently with fresh provider")
                        break
            self.untested("desktop_composer_UI", "HTTP validates no auto-send and explicit send semantics; no browser/desktop interaction in this script")
            if not any(name.startswith(("files-", "workspace-")) for name in selected):
                self.untested("file_restore", "not selected in this run")
            require(not self.model.errors, self.model.errors)
            self.report["success"] = not self.report["failures"]
            self.report["status"] = "passed_with_uncovered_items" if self.report["success"] else "failed"
            return 0 if self.report["success"] else 1
        except Exception as e:
            self.report["status"] = "failed"
            self.report["failures"].append({"scenario": "setup_or_fixture", "error": str(e), "traceback": traceback.format_exc()})
            return 1
        finally:
            self.close()
            print(json.dumps({"status": self.report["status"], "report": str(self.root / "report.json")}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", help="Already-built Codeg server binary; never invokes cargo")
    parser.add_argument("--output", required=True, help="Audit workspace root; unique run directory is retained")
    parser.add_argument("--npm-root", help="Installed node_modules containing @agentclientprotocol")
    parser.add_argument("--codex-native", help="Installed native Codex executable")
    parser.add_argument("--sidecar-dir", help="Directory containing real release codeg-mcp.exe and codeg-computer-helper.exe")
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--scenarios", default="codex-live,codex-plain,claude,queue", help="Comma-separated codex-live,codex-replay,codex-plain,claude,queue,files-codex,files-claude,workspace-codex,workspace-claude,workspace-overlap")
    args = parser.parse_args()
    return Audit(args).run()


if __name__ == "__main__":
    sys.exit(main())
