//! Background translation service using Google Translate GTX endpoint.

#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

/// Result of a translation attempt: (translated_text, detected_language_code).
pub type TranslationResult = Result<(String, String), String>;

#[cfg(not(target_arch = "wasm32"))]
fn os_roots() -> Result<ureq::tls::RootCerts, String> {
    let found = rustls_native_certs::load_native_certs();
    let certs: Vec<ureq::tls::Certificate<'static>> = found.certs.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
    if certs.is_empty() {
        return Err("no trusted certificates found on this system".into());
    }
    Ok(ureq::tls::RootCerts::new_with_certs(&certs))
}

/// Longest text sent, in characters: about two dense pages. The popup cuts longer selections
/// with [`cap`] and says so.
pub const MAX_CHARS: usize = 20_000;
/// Characters per request: the endpoint answers 400 to much longer bodies.
const CHUNK_CHARS: usize = 4_500;

/// Request translation for `query` from `sl` (source language, e.g. "auto")
/// into `tl` (target language, e.g. "tr").
///
/// The text goes in the POST body, never in the URL, and in pieces of at most [`CHUNK_CHARS`]
/// characters, so a long selection doesn't hit the URL length limit (HTTP 400).
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_translation(query: &str, sl: &str, tl: &str) -> TranslationResult {
    let url = format!("https://translate.googleapis.com/translate_a/single?client=gtx&sl={sl}&tl={tl}&dt=t");

    let mut builder = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30)));

    if let Ok(roots) = os_roots() {
        builder = builder.tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build());
    }

    let agent = builder.build().new_agent();

    let mut translated = String::new();
    let mut detected: Option<String> = None;
    for piece in chunks(query, CHUNK_CHARS) {
        let mut resp = agent
            .post(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send_form([("q", piece)])
            .map_err(|e| format!("Network request failed: {e}"))?;

        let body = resp.body_mut().read_to_string().map_err(|e| format!("Failed to read response: {e}"))?;
        let (part, lang) = parse_gtx_json(&body)?;
        translated.push_str(&part);
        detected = detected.or(Some(lang));
    }

    if translated.is_empty() {
        return Err("No translation returned".into());
    }
    Ok((translated, detected.unwrap_or_else(|| "auto".to_string())))
}

#[cfg(target_arch = "wasm32")]
pub fn fetch_translation(_query: &str, _sl: &str, _tl: &str) -> TranslationResult {
    Err("Translation is not supported in the web build".into())
}

/// Parse Google Translate GTX JSON format:
/// `[[["Translated text", "Original text", ...], ...], null, "detected_lang", ...]`
fn parse_gtx_json(json_str: &str) -> TranslationResult {
    let value: serde_json::Value = serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON response: {e}"))?;

    let sentences = value.get(0).and_then(|v| v.as_array()).ok_or_else(|| "Unexpected response structure".to_string())?;

    let mut translated = String::new();
    for sentence in sentences {
        if let Some(part) = sentence.get(0).and_then(|p| p.as_str()) {
            translated.push_str(part);
        }
    }

    if translated.is_empty() {
        return Err("No translation returned".into());
    }

    let detected_lang = value.get(2).and_then(|v| v.as_str()).unwrap_or("auto").to_string();

    Ok((translated, detected_lang))
}

/// At most `max` characters of `text`, cut at a char boundary, and whether anything was cut.
pub fn cap(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (text.get(..at).unwrap_or_default().to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// Rejoin the lines of a PDF selection into paragraphs, so the translation doesn't break where
/// the page's lines did: a blank line ends a paragraph, a word hyphenated across lines is joined,
/// and every other line break becomes a space.
pub fn unwrap_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            if blank {
                out.push_str("\n\n");
            } else if out.ends_with('-') && line.starts_with(char::is_lowercase) {
                out.pop();
            } else {
                out.push(' ');
            }
        }
        blank = false;
        out.push_str(line);
    }
    out
}

/// Split `text` into pieces of at most `max` characters, preferring to cut after a newline, then
/// after a sentence end, then after a space. Every piece is non-empty and they concatenate back to
/// `text`.
fn chunks(text: &str, max: usize) -> Vec<&str> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // The byte offset just past the `max`-th character, or the whole rest.
        let Some((limit, _)) = rest.char_indices().nth(max) else {
            out.push(rest);
            break;
        };
        let window = rest.get(..limit).unwrap_or(rest);
        let cut = window
            .rfind('\n')
            .or_else(|| window.rfind(". "))
            .or_else(|| window.rfind(' '))
            // Past the separator (all one-byte ASCII), so the piece keeps it; never an empty piece.
            .map(|i| i.saturating_add(1))
            .filter(|&i| i > 0 && rest.is_char_boundary(i))
            .unwrap_or(limit);
        let (piece, tail) = rest.split_at(cut);
        out.push(piece);
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_rebuild_the_text_and_respect_the_limit() {
        let text = "First line.\nSecond sentence. Third word àèìòù 日本語 end\n".repeat(40);
        for max in [1, 7, 30, 100, 10_000] {
            let parts = chunks(&text, max);
            assert_eq!(parts.concat(), text);
            assert!(parts.iter().all(|p| !p.is_empty() && p.chars().count() <= max), "max {max}");
        }
        assert!(chunks("", 10).is_empty());
    }

    #[test]
    fn unwrap_lines_rebuilds_paragraphs() {
        assert_eq!(
            unwrap_lines("zero-dimensional data (i.e. point\r\ndata) as op-\nposed\n\n  Next para  \n"),
            "zero-dimensional data (i.e. point data) as opposed\n\nNext para"
        );
        assert_eq!(unwrap_lines("A-\nB"), "A- B", "a capital after the hyphen is a new word");
        assert_eq!(unwrap_lines(""), "");
        assert_eq!(unwrap_lines("\n\n\n"), "");
    }

    #[test]
    fn cap_cuts_at_char_boundaries() {
        assert_eq!(cap("àèì", 2), ("àè".to_owned(), true));
        assert_eq!(cap("àèì", 3), ("àèì".to_owned(), false));
        assert_eq!(cap("", 0), (String::new(), false));
    }

    #[test]
    fn test_parse_gtx_json() {
        let sample = r#"[[["PdfCraft bir PDF çalışma tezgahıdır. ","PdfCraft is a PDF workbench. ",null,null,3],["Saf Rust ile yazılmıştır.","It is written in pure Rust.",null,null,3]],null,"en"]"#;
        let res = parse_gtx_json(sample);
        if let Ok((translated, detected)) = res {
            assert_eq!(translated, "PdfCraft bir PDF çalışma tezgahıdır. Saf Rust ile yazılmıştır.");
            assert_eq!(detected, "en");
        } else {
            assert!(false, "parse failed");
        }
    }
}
