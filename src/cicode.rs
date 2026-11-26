//! Análisis liviano de Cicode: tokens, índice de funciones, chequeo estructural de bloques y
//! control de expresiones permitidas en modo solo lectura. No reemplaza compilar en Studio.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Punct(char),
    Str,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: usize,
}

#[derive(Debug, Default)]
struct Lexed {
    tokens: Vec<Token>,
    errors: Vec<String>,
}

/// Separa en palabras, signos y literales; descarta comentarios (`//`, `!`, `/* */`).
fn lex(src: &str) -> Lexed {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Lexed::default();
    let (mut i, mut line) = (0usize, 1usize);
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '!' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let start = line;
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    out.errors.push(format!("línea {start}: comentario /* sin cerrar"));
                }
                i += 2;
            }
            '"' => {
                let start = line;
                i += 1;
                let mut closed = false;
                while i < chars.len() && chars[i] != '\n' {
                    // ^ es el carácter de escape de Cicode (^", ^n, ^t, ^^).
                    if chars[i] == '^' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    out.errors.push(format!("línea {start}: cadena sin cerrar"));
                }
                out.tokens.push(Token { tok: Tok::Str, line: start });
            }
            c if c.is_alphabetic() || c == '_' => {
                let s = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                out.tokens.push(Token { tok: Tok::Word(chars[s..i].iter().collect::<String>().to_uppercase()), line });
            }
            c if c.is_whitespace() => i += 1,
            c => {
                out.tokens.push(Token { tok: Tok::Punct(c), line });
                i += 1;
            }
        }
    }
    out
}

fn word(t: Option<&Token>) -> Option<&str> {
    match t.map(|t| &t.tok) {
        Some(Tok::Word(w)) => Some(w.as_str()),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub line: usize,
    /// "PUBLIC" (por defecto), "PRIVATE"...
    pub scope: String,
    /// Tipo de retorno ("VOID" si no se declara).
    pub returns: String,
}

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub ok: bool,
    pub errors: Vec<String>,
    pub functions: Vec<FunctionDef>,
}

const TYPES: &[&str] = &["INT", "REAL", "STRING", "OBJECT", "LONG", "QUALITY", "TIMESTAMP"];

/// Funciones declaradas en un archivo .ci.
pub fn functions(src: &str) -> Vec<FunctionDef> {
    let lx = lex(src);
    let t = &lx.tokens;
    let mut out = Vec::new();
    for (i, tk) in t.iter().enumerate() {
        if tk.tok != Tok::Word("FUNCTION".into()) {
            continue;
        }
        let Some(name) = word(t.get(i + 1)) else { continue };
        let prev: Vec<&str> = t[i.saturating_sub(2)..i].iter().filter(|p| p.line == tk.line).filter_map(|p| word(Some(p))).collect();
        let returns = prev.iter().find(|w| TYPES.contains(w)).map(|s| s.to_string()).unwrap_or_else(|| "VOID".into());
        let scope = prev.iter().find(|w| ["PUBLIC", "PRIVATE"].contains(w)).map(|s| s.to_string()).unwrap_or_else(|| "PUBLIC".into());
        // Recupera el nombre con sus mayúsculas originales.
        let original = src
            .lines()
            .nth(tk.line - 1)
            .and_then(|l| {
                let up = l.to_uppercase();
                let pos = up.find("FUNCTION")? + "FUNCTION".len();
                let rest = l[pos..].trim_start();
                Some(rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>())
            })
            .filter(|s| s.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| name.to_string());
        out.push(FunctionDef { name: original, line: tk.line, scope, returns });
    }
    out
}

/// Chequeo estructural: bloques FUNCTION/IF/WHILE/FOR/SELECT CASE/REPEAT, paréntesis y cadenas.
pub fn check(src: &str) -> CheckReport {
    let lx = lex(src);
    let mut errors = lx.errors.clone();
    let t = &lx.tokens;
    let mut stack: Vec<(&'static str, usize)> = Vec::new();
    let mut parens: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let tk = &t[i];
        match &tk.tok {
            Tok::Punct('(') => parens.push(tk.line),
            Tok::Punct(')') => {
                if parens.pop().is_none() {
                    errors.push(format!("línea {}: ')' sin '(' correspondiente", tk.line));
                }
            }
            Tok::Word(w) => match w.as_str() {
                "FUNCTION" => {
                    if let Some((kind, line)) = stack.first() {
                        errors.push(format!("línea {}: FUNCTION dentro de otro bloque ({kind} abierto en línea {line} sin END)", tk.line));
                        stack.clear();
                    }
                    stack.push(("FUNCTION", tk.line));
                }
                "IF" => stack.push(("IF", tk.line)),
                "WHILE" => stack.push(("WHILE", tk.line)),
                "FOR" => stack.push(("FOR", tk.line)),
                "REPEAT" => stack.push(("REPEAT", tk.line)),
                "SELECT" if word(t.get(i + 1)) == Some("CASE") && t[i + 1].line == tk.line => {
                    stack.push(("SELECT CASE", tk.line));
                    i += 1;
                }
                "UNTIL" => match stack.pop() {
                    Some(("REPEAT", _)) => {}
                    Some((k, l)) => errors.push(format!("línea {}: UNTIL pero el bloque abierto es {k} (línea {l})", tk.line)),
                    None => errors.push(format!("línea {}: UNTIL sin REPEAT", tk.line)),
                },
                "END" => {
                    let select = word(t.get(i + 1)) == Some("SELECT") && t[i + 1].line == tk.line;
                    if select {
                        i += 1;
                    }
                    match stack.pop() {
                        Some(("SELECT CASE", _)) if select => {}
                        Some(("SELECT CASE", l)) => {
                            errors.push(format!("línea {}: se esperaba END SELECT para el SELECT CASE de la línea {l}", tk.line))
                        }
                        Some((k, l)) if select => errors.push(format!("línea {}: END SELECT pero el bloque abierto es {k} (línea {l})", tk.line)),
                        Some(("REPEAT", l)) => errors.push(format!("línea {}: END pero el REPEAT de la línea {l} se cierra con UNTIL", tk.line)),
                        Some(_) => {}
                        None => errors.push(format!("línea {}: END sin bloque abierto", tk.line)),
                    }
                }
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    for (k, l) in stack {
        errors.push(format!("línea {l}: {k} sin cerrar (falta {})", match k {
            "SELECT CASE" => "END SELECT",
            "REPEAT" => "UNTIL",
            _ => "END",
        }));
    }
    for l in parens {
        errors.push(format!("línea {l}: '(' sin cerrar"));
    }
    CheckReport { ok: errors.is_empty(), errors, functions: functions(src) }
}

/// Prefijos de funciones Cicode de solo lectura (consultas, browse, conversión de texto/tiempo).
const READ_PREFIXES: &[&str] = &[
    "tagread", "taggetproperty", "taggetscale", "taginfo", "tagbrowse", "trnbrowse", "trninfo", "almbrowse", "almsummary",
    "alarmgetfield", "alarmfirst", "alarmnext", "alarmcount", "alarmgetinfo", "alarmcatgetformat", "equipbrowse",
    "equipgetproperty", "time", "date", "str", "int", "real", "char", "version", "projectinfo", "projectcurrentget", "parameterget", "serverinfo",
    "serverisonline", "clusterstatus", "clustergetname", "clusterfirst", "clusternext", "iodeviceinfo", "userinfo",
    "fullname", "wndinfo", "errmsg", "iserror", "abs", "max", "min", "round", "sqrt", "pow", "getenv", "getpriv",
    "getarea", "sysinfo",
];
/// Palabras que delatan efectos aunque el prefijo sea de lectura (p. ej. AlmBrowseAck, TimeSet).
const DENY_WORDS: &[&str] = &[
    "ack", "write", "set", "delete", "disable", "enable", "clear", "reset", "control", "shutdown", "exec", "kill", "remove",
    "insert", "add", "save", "print", "display", "silence", "shelve", "login", "logout", "create",
];

/// En modo solo lectura, devuelve por qué la expresión no está permitida (o None si lo está).
pub fn readonly_violation(expr: &str) -> Option<String> {
    let lx = lex(expr);
    let t = &lx.tokens;
    // Una expresión sin llamadas (p. ej. el nombre de un tag) solo lee.
    for (i, tk) in t.iter().enumerate() {
        match &tk.tok {
            Tok::Punct('=') => return Some("contiene '=' (asignación)".into()),
            Tok::Word(w) if t.get(i + 1).map(|n| &n.tok) == Some(&Tok::Punct('(')) => {
                let lw = w.to_lowercase();
                let allowed = READ_PREFIXES.iter().any(|p| lw.starts_with(p)) && !DENY_WORDS.iter().any(|d| lw.contains(d));
                if !allowed {
                    return Some(format!("la función {w} no está en la lista de funciones de solo lectura"));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
// Control de interruptor
PUBLIC INT FUNCTION BreakerOpen(STRING sTag)
    INT iRes = 0;
    IF TagRead(sTag + "\XCBR1\Pos") = 1 THEN
        iRes = TagWrite(sTag + "\CSWI1\Pos\Oper", 0);  ! comentario con IF
    ELSE
        iRes = -1;
    END
    SELECT CASE iRes
    CASE 0
        Message("OK", "Abierto ^"bien^"", 0);
    CASE ELSE
        WHILE iRes < 3 DO
            iRes = iRes + 1;
        END
    END SELECT
    /* FOR sin cerrar en comentario
       FOR i = 0 TO 3 DO */
    RETURN iRes;
END

PRIVATE FUNCTION Helper()
    REPEAT
        Sleep(1);
    UNTIL TRUE
END
"#;

    #[test]
    fn good_code_passes() {
        let r = check(GOOD);
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.functions.len(), 2);
        assert_eq!(r.functions[0].name, "BreakerOpen");
        assert_eq!(r.functions[0].returns, "INT");
        assert_eq!(r.functions[1].scope, "PRIVATE");
        assert_eq!(r.functions[1].returns, "VOID");
    }

    #[test]
    fn detects_missing_end() {
        let r = check("FUNCTION A()\n IF x THEN\n y = 1;\nEND\n\nFUNCTION B()\nEND\n");
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("línea 6")), "{:?}", r.errors);
        let r = check("FUNCTION A()\n SELECT CASE x\n CASE 1\n END\nEND\n");
        assert!(r.errors.iter().any(|e| e.contains("END SELECT")), "{:?}", r.errors);
        let r = check("FUNCTION A()\n Message(\"hola);\nEND\n");
        assert!(r.errors.iter().any(|e| e.contains("cadena sin cerrar")), "{:?}", r.errors);
    }

    #[test]
    fn readonly_rules() {
        assert!(readonly_violation(r#"TagRead("Feeder1\MMXU1\TotW")"#).is_none());
        assert!(readonly_violation(r#"StrToInt(TagRead("X"))"#).is_none());
        assert!(readonly_violation(r#"AlmBrowseOpen("TAG=*", "TAG,STATE", "", "")"#).is_none());
        assert!(readonly_violation(r#"TagWrite("X", 1)"#).is_some());
        assert!(readonly_violation(r#"AlmBrowseAck(1)"#).is_some());
        assert!(readonly_violation(r#"TimeSet(0)"#).is_some());
        assert!(readonly_violation("X = 1").is_some());
        assert!(readonly_violation(r#"TagRead("A=B")"#).is_none());
    }
}
