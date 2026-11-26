"""Prueba del camino CtAPI completo contra un CtApi.dll falso (escrituras habilitadas).
Correr después de offline.py (crea el proyecto falso) y de compilar tests/fakectapi (cargo build --release)."""
import json, os, subprocess, sys, tempfile

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
EXE = os.path.join(REPO, "target", "release", "poweroperation-mcp.exe")
HERE = os.path.join(tempfile.gettempdir(), "pomcp-e2e")
DLLDIR = os.path.join(REPO, "tests", "fakectapi", "target", "release")
AUDIT = os.path.join(HERE, "audit-test.log")
if os.path.exists(AUDIT):
    os.remove(AUDIT)

env = dict(os.environ, POWEROP_CTAPI=DLLDIR, POWEROP_ALLOW_WRITE="1", POWEROP_TIMEOUT_SECS="10",
           POWEROP_AUDIT_LOG=AUDIT, POWEROP_PROJECTS_DIR=os.path.join(HERE, "fakeuser"))
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
    return None if notify else json.loads(p.stdout.readline())


def call(name, args=None):
    res = rpc("tools/call", {"name": name, "arguments": args or {}}).get("result", {})
    return res.get("isError", False), "".join(c.get("text", "") for c in res.get("content", []))


rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
rpc("notifications/initialized", notify=True)
ok = []
def check(label, cond, detail=""):
    ok.append(cond); print(("PASS " if cond else "FAIL ") + label + ("" if cond else f"\n    {detail[:800]}"))

e, t = call("runtime_status"); check("runtime_status conecta", "Conexión CtAPI: OK (hora del SCADA: 10:15:00)" in t, t)
check("runtime_status muestra proyecto en runtime", "Proyecto en runtime: PO_Demo" in t, t)
e, t = call("project_query", {"table": "variable", "limit": 1}); check("proyecto por defecto = el del runtime", not e and '"matched": 3' in t, t)
e, t = call("alarm_sql", {"sql": "select * from CiAlarmObject"}); check("alarm_sql informa sonda TCP", e and "puerto TCP" in t, t)
e, t = call("tag_read", {"tags": ["Feeder1\\MMXU1\\TotW", "NoExiste"]}); j = json.loads(t)
check("tag_read valor", j[0]["value"] == "1234.5", t)
check("tag_read error Citect decodificado", "error Citect 257" in j[1]["error"] and "0x10000101" in j[1]["error"], t)
e, t = call("tag_write", {"tag": "Feeder1\\XCBR1\\Pos", "value": "0"}); j = json.loads(t)
check("tag_write escribe y relee", j["before"] == "2" and j["after"] == "0", t)
e, t = call("tag_browse", {"filter": "TAG=Feeder1*", "with_values": True}); j = json.loads(t)
check("tag_browse filtra, lee valores y quita props inexistentes",
      j["count"] == 2 and j["rows"][0]["VALUE"] == "1234.5" and "UNIT" in j["unavailable_properties"], t)
e, t = call("tag_browse", {"filter": "TAG=Nada*"}); j = json.loads(t)
check("tag_browse vacío con nota", j["count"] == 0 and "error Citect 275" in j["note"], t)
e, t = call("trend_query", {"tag": "Feeder1_kW", "end": "2026-10-14 10:00", "minutes_back": 10, "period_seconds": 60}); j = json.loads(t)
check("trend_query 10 muestras", j["returned"] == 10 and j["rows"][-1][0].startswith("2026-10-14 10:00:00"), t)
check("trend_query estadísticas solo con calidad buena", j["stats"]["good_samples"] == 9 and j["stats"]["min"] == 1.5 and j["rows"][0][2] == "bad", t)
e, t = call("trend_query", {"tag": "X", "minutes_back": 5, "period_seconds": 60, "format": "csv"})
check("trend_query CSV", "time,value,quality" in t and t.count("\n") > 5, t)
e, t = call("alarm_list"); j = json.loads(t); check("alarm_list", j["rows"][0]["TAG"] == "Feeder1_Trip", t)
e, t = call("alarm_ack", {"tag": 'Feeder1_"Trip'}); check("alarm_ack con escape Cicode", 'AlarmAckTag("Feeder1_^"Trip", "") → 0' in t, t)
e, t = call("cicode_run", {"command": 'TagWrite("X", 1)'}); check("cicode_run con efectos permitido con ALLOW_WRITE", t == 'eval:TagWrite("X", 1)', t)
e, t = call("cicode_run", {"command": 'IntToStr(1)'}); check("cicode_run lectura", t == "eval:IntToStr(1)", t)
e, t = call("ctapi_find", {"table": "Tag", "properties": ["tag", "type"], "limit": 2}); j = json.loads(t)
check("ctapi_find con limit y props en mayúsculas", j["count"] == 2 and j.get("truncated") is True and j["rows"][0]["TYPE"] == "REAL", t)
e, t = call("runtime_status", {"reconnect": True}); check("reconnect", "OK" in t, t)

w = "x-new.ci"
e, t = call("cicode_write", {"project": "PO_Demo", "file": w, "content": "FUNCTION Z()\nEND\n"}); check("cicode_write crea", not e and "creado" in t, t)
e, t = call("cicode_write", {"project": "PO_Demo", "file": w, "content": "FUNCTION Z()\n IF 1 THEN\nEND\n"})
check("cicode_write reemplaza con backup y reporta chequeo", "_mcp_backup" in t and '"ok": false' in t, t)
raw = open(os.path.join(HERE, "fakeuser", "PO_Demo", w), "rb").read(); check("cicode_write CRLF", b"\r\n" in raw and b"\r\r" not in raw, repr(raw))
audit = open(AUDIT, encoding="utf-8").read()
check("auditoría registra escrituras", all(k in audit for k in ["tag_write", "alarm_ack", "cicode_run", "cicode_write"]), audit)

p.stdin.close(); p.wait(timeout=10)
print(f"\n{sum(ok)}/{len(ok)} OK")
sys.exit(0 if all(ok) else 1)
