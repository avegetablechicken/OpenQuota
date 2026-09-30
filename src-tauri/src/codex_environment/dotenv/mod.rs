// dotenvy 0.15.7 syntax with a virtual environment instead of process mutation.
mod lines;
mod parse;

use std::collections::HashMap;

pub(super) fn load(
    reader: impl std::io::Read,
    process: impl Fn(&str) -> Option<String>,
) -> HashMap<String, String> {
    let mut applied = HashMap::<String, String>::new();
    let mut substitutions = HashMap::new();
    for line in lines::QuotedLines::new(reader).flatten() {
        let lookup = |name: &str| {
            if name.is_empty() {
                return None;
            }
            applied.get(name).cloned().or_else(|| process(name))
        };
        if let Ok(Some((key, value))) = parse::parse_line(&line, &mut substitutions, &lookup) {
            // Codex filters after parsing, so rejected keys still exist in the parser's
            // substitution table, but never override the virtual process environment.
            if !key.to_ascii_uppercase().starts_with("CODEX_") {
                applied.insert(key, value);
            }
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulates_sequential_overrides_without_touching_the_process() {
        let inherited = HashMap::from([
            ("PROXY_PORT", "7890"),
            ("SELF", "seed"),
            ("NO_PROXY", "inherited"),
            ("CODEX_KEEP", "original"),
        ]);
        let before = std::env::vars_os().collect::<HashMap<_, _>>();
        let values = load(
            br#"BEFORE=${PROXY_PORT}
PROXY_PORT=8080
HTTPS_PROXY=http://127.0.0.1:${PROXY_PORT}
PROXY_PORT=9090
ALL_PROXY=http://127.0.0.1:${PROXY_PORT}
SELF=${SELF}-next
SELF=${SELF}-again
NO_PROXY=
EMPTY=${NO_PROXY}
LITERAL='${PROXY_PORT}'
ESCAPED=\${PROXY_PORT}
CODEX_KEEP=ignored
FILTERED=${CODEX_KEEP}
CODEX_NEW=parser-only
NEW_REFERENCE=${CODEX_NEW}
INVALID LINE
AFTER=valid
"#
            .as_slice(),
            |name| inherited.get(name).map(|value| (*value).to_owned()),
        );
        for (name, expected) in [
            ("BEFORE", "7890"),
            ("HTTPS_PROXY", "http://127.0.0.1:8080"),
            ("ALL_PROXY", "http://127.0.0.1:9090"),
            ("SELF", "seed-next-again"),
            ("EMPTY", ""),
            ("LITERAL", "${PROXY_PORT}"),
            ("ESCAPED", "${PROXY_PORT}"),
            ("FILTERED", "original"),
            ("NEW_REFERENCE", "parser-only"),
            ("AFTER", "valid"),
        ] {
            assert_eq!(
                values.get(name).map(String::as_str),
                Some(expected),
                "{name}"
            );
        }
        assert!(!values.contains_key("CODEX_KEEP"));
        assert!(!values.contains_key("CODEX_NEW"));
        assert_eq!(std::env::vars_os().collect::<HashMap<_, _>>(), before);
    }

    #[test]
    fn syntax_and_error_recovery_match_dotenvy() {
        for text in [
            "# comment\nexport KEY=one # comment\nKEY=two\n",
            "KEY=\"first\nsecond\"\nNEXT='literal # text'\n",
            "KEY=\"escaped\\nvalue\"\nBAD=\\q\nAFTER=ok\n",
            "INVALID LINE\nKEY=a=b#c\nEMPTY= # comment\n",
            "export=value\nDOTTED.KEY=one\nKEY=one two\nAFTER=ok\n",
            "KEY='unterminated\nAFTER=not-a-new-assignment\n",
            "\u{feff}KEY=bom\nAFTER=ok\r\n",
        ] {
            let expected = dotenvy::from_read_iter(text.as_bytes())
                .flatten()
                .collect::<HashMap<_, _>>();
            assert_eq!(load(text.as_bytes(), |_| None), expected, "{text:?}");
        }
    }
}
