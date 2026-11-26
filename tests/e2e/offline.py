"""Prueba end-to-end del MCP por stdio con un proyecto falso (requiere cargo build --release)."""
import json, os, struct, subprocess, sys, shutil, tempfile

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
EXE = os.path.join(REPO, "target", "release", "poweroperation-mcp.exe")
ROOT = os.path.join(tempfile.gettempdir(), "pomcp-e2e", "fakeuser")


def dbf(path, fields, rows):
    rec_len = 1 + sum(l for _, l in fields)
    hdr_len = 32 + 32 * len(fields) + 1
    out = bytearray(struct.pack("<BBBBIHH20x", 3, 126, 10, 14, len(rows), hdr_len, rec_len))
    for name, l in fields:
        out += name.encode().ljust(11, b"\0") + b"C" + b"\0" * 4 + bytes([l, 0]) + b"\0" * 14
    out += b"\r"
    for r in rows:
        out += b" " + b"".join(v.encode("cp1252").ljust(l, b" ")[:l] for v, (_, l) in zip(r, fields))
    out += b"\x1a"
    open(path, "wb").write(out)


shutil.rmtree(ROOT, ignore_errors=True)
main = os.path.join(ROOT, "PO_Demo"); lib = os.path.join(ROOT, "PLS_Include")
os.makedirs(main); os.makedirs(lib)
dbf(os.path.join(main, "variable.dbf"), [("TAG", 60), ("TYPE", 10), ("UNIT", 16), ("ENG_UNITS", 8), ("COMMENT", 48)], [
    ("Feeder1\\MMXU1\\TotW", "REAL", "Feeder1", "kW", "Potencia activa total"),
    ("Feeder1\\XCBR1\\Pos", "DIGITAL", "Feeder1", "", "Posición interruptor"),
    ("Feeder2\\MMXU1\\TotW", "REAL", "Feeder2", "kW", ""),
])
dbf(os.path.join(main, "digalm.dbf"), [("TAG", 40), ("NAME", 40), ("VAR_A", 60), ("CATEGORY", 8)], [
    ("Feeder1_Trip", "Disparo Feeder1", "Feeder1\\XCBR1\\Pos", "1"),
])
dbf(os.path.join(main, "units.dbf"), [("NAME", 16), ("PROTOCOL", 16), ("PORT", 16)], [("Feeder1", "MODNET", "PORT1_TCP")])
dbf(os.path.join(main, "include.dbf"), [("NAME", 40)], [("PLS_Include",)])
dbf(os.path.join(lib, "variable.dbf"), [("TAG", 60), ("COMMENT", 40)], [("PLS_Heartbeat", "Latido")])
open(os.path.join(main, "Feeders.ci"), "wb").write(
    "// Lee potencia\r\nREAL FUNCTION FeederPower(STRING sDev)\r\n  RETURN StrToReal(TagRead(sDev + \"\\MMXU1\\TotW\"));\r\nEND\r\n".encode("cp1252"))
open(os.path.join(lib, "PlsLib.ci"), "wb").write(b"PRIVATE FUNCTION PlsInit()\r\nEND\r\n")
# Copia vieja de citect.ini dentro de un proyecto (como en PO 2024): debe ignorarse.
open(os.path.join(main, "citect.ini"), "w").write(f"[CtEdit]\nRun={lib}\nUser={ROOT}\n")

env = dict(os.environ, POWEROP_PROJECTS_DIR=ROOT, POWEROP_PROJECT="PO_Demo", POWEROP_TIMEOUT_SECS="10",
           POWEROP_CTAPI=os.path.join(ROOT, "no-existe"))
p = subprocess.Popen([EXE], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
n = 0


def rpc(method, params=None, notify=False):
    global n
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not notify:
        n += 1; msg["id"] = n
    p.stdin.write((json.dumps(msg) + "\n").encode()); p.stdin.flush()
    if notify:
        return None
    return json.loads(p.stdout.readline())


def call(name, args=None):
    r = rpc("tools/call", {"name": name, "arguments": args or {}})
    res = r.get("result", {})
    text = "".join(c.get("text", "") for c in res.get("content", []))
    return res.get("isError", False), text


init = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
print("server:", init["result"]["serverInfo"])
rpc("notifications/initialized", notify=True)
tools = rpc("tools/list")["result"]["tools"]
print(len(tools), "tools:", ", ".join(t["name"] for t in tools))

checks = []
def check(label, cond, detail=""):
    checks.append(cond); print(("PASS " if cond else "FAIL ") + label + ("" if cond else f"\n    {detail[:600]}"))

e, t = call("project_list"); j = json.loads(t)
check("project_list encuentra 2 proyectos", {x["name"] for x in j["projects"]} == {"PO_Demo", "PLS_Include"}, t)
check("citect.ini dentro de proyecto ignorado", not any(x["running"] for x in j["projects"]) and j["citect_ini"] == [], t)
e, t = call("project_tables"); check("project_tables lista variable", '"variable"' in t and "TAG(C60)" in t, t)
e, t = call("project_query", {"table": "variable", "where": {"TAG": "Feeder1*"}, "fields": ["TAG", "COMMENT"]})
j = json.loads(t); check("project_query filtra y decodifica cp1252", j["matched"] == 2 and j["rows"][1]["COMMENT"] == "Posición interruptor", t)
e, t = call("project_query", {"table": "variable", "text": "heartbeat", "include_included": True})
check("project_query en incluidos", '"_project": "PLS_Include"' in t, t)
e, t = call("project_query", {"table": "variable", "where": {"NOPE": "x"}}); check("campo inexistente da error claro", e and "Campos:" in t, t)
e, t = call("project_search", {"text": "XCBR1"}); j = json.loads(t)
check("project_search encuentra en variable y digalm", {h.get("table") for h in j["hits"]} >= {"variable", "digalm"}, t)
e, t = call("cicode_files", {"include_included": True}); check("cicode_files lista funciones", "FeederPower (l.2)" in t and "PlsInit" in t, t)
e, t = call("cicode_read", {"file": "Feeders", "function": "FeederPower", "numbered": True})
check("cicode_read extrae función con comentario", "1| // Lee potencia" in t and "4| END" in t, t)
e, t = call("cicode_read", {"file": "PlsLib.ci"}); check("cicode_read busca en incluidos", "PLS_Include" in t, t)
e, t = call("cicode_check", {"code": "FUNCTION A()\n IF x THEN\nEND\n"}); check("cicode_check detecta IF sin END", '"ok": false' in t, t)
e, t = call("cicode_write", {"file": "Nuevo.ci", "content": "FUNCTION X()\nEND\n"}); check("cicode_write bloqueado en solo lectura", e and "POWEROP_ALLOW_WRITE" in t, t)
e, t = call("tag_write", {"tag": "Feeder1\\XCBR1\\Pos", "value": "0"}); check("tag_write bloqueado en solo lectura", e and "solo lectura" in t, t)
e, t = call("cicode_run", {"command": 'TagWrite("X", 1)'}); check("cicode_run con efectos bloqueado", e and "TAGWRITE" in t.upper(), t)
e, t = call("cicode_run", {"command": 'TagRead("X")'}); check("cicode_run de lectura pasa el filtro y falla por CtApi.dll", e and "CtApi.dll" in t, t)
e, t = call("trend_query", {"tag": "A,B"}); check("trend_query valida comas", e, t)
e, t = call("runtime_status"); check("runtime_status informa sin DLL", "NO encontrado" in t or "candidato" in t, t); print("    ---\n" + "\n".join("    " + l for l in t.splitlines()))
e, t = call("poweroperation_reference"); check("reference", "TRNQUERY" in t and "CiAlarmObject" in t, t[:200])

p.stdin.close(); p.wait(timeout=10)
print(f"\n{sum(checks)}/{len(checks)} OK")
sys.exit(0 if all(checks) else 1)
