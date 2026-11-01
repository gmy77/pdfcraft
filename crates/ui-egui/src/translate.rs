//! Translate the selected text through Google Translate's public endpoint (fork-only feature:
//! upstream allows no outbound network). The request runs on a worker thread; the window polls it
//! each frame, so the interface never waits on the network. Nothing here may panic: the text comes
//! from the PDF and the reply from the network, both untrusted.

use std::sync::mpsc::{Receiver, TryRecvError};

/// Longest selection sent, in characters: about two dense pages. Longer text is cut, and the
/// window says so.
const MAX_CHARS: usize = 20_000;
/// Characters per request; the endpoint refuses much longer bodies.
const CHUNK_CHARS: usize = 4_500;

/// Target languages offered: (code, native name). The first one is the default.
const LANGUAGES: &[(&str, &str)] = &[
    ("it", "Italiano"),
    ("en", "English"),
    ("de", "Deutsch"),
    ("fr", "Français"),
    ("es", "Español"),
    ("pt", "Português"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("ru", "Русский"),
    ("uk", "Українська"),
    ("zh-CN", "中文"),
    ("ja", "日本語"),
    ("ar", "العربية"),
];

type Reply = Result<Translated, String>;

struct Translated {
    text: String,
    /// The language Google detected, e.g. "en".
    detected: Option<String>,
}

enum State {
    Idle,
    Busy(Receiver<Reply>),
    Done(Translated),
    Failed(String),
}

/// The translation window of one document view.
pub(crate) struct Translator {
    open: bool,
    source: String,
    truncated: bool,
    target: usize,
    state: State,
}

impl Default for Translator {
    fn default() -> Self {
        Self { open: false, source: String::new(), truncated: false, target: 0, state: State::Idle }
    }
}

impl Translator {
    /// Open the window and translate `text` into the chosen language.
    pub(crate) fn start(&mut self, ctx: &egui::Context, text: &str) {
        // The language chosen last time, in this or an earlier session.
        if let Some(code) = ctx.data_mut(|d| d.get_persisted::<String>(target_id()))
            && let Some(i) = LANGUAGES.iter().position(|(c, _)| *c == code)
        {
            self.target = i;
        }
        let (source, truncated) = cap(text.trim(), MAX_CHARS);
        self.source = source;
        self.truncated = truncated;
        self.open = true;
        self.request(ctx);
    }

    fn request(&mut self, ctx: &egui::Context) {
        if self.source.is_empty() {
            self.state = State::Idle;
            return;
        }
        let target = LANGUAGES.get(self.target).map_or("it", |(code, _)| code);
        self.state = match spawn(ctx.clone(), unwrap_lines(&self.source), target) {
            Ok(rx) => State::Busy(rx),
            Err(e) => State::Failed(e),
        };
    }

    fn poll(&mut self) {
        if let State::Busy(rx) = &self.state {
            self.state = match rx.try_recv() {
                Ok(Ok(done)) => State::Done(done),
                Ok(Err(e)) => State::Failed(e),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => State::Failed(tl!("The translation stopped unexpectedly.").to_owned()),
            };
        }
    }
}

/// Where egui's persisted memory keeps the target language's code.
fn target_id() -> egui::Id {
    egui::Id::unique("translate-target-language")
}

/// Draw the translation window, if it is open.
pub(crate) fn show(ctx: &egui::Context, id: egui::Id, tr: &mut Translator) {
    if !tr.open {
        return;
    }
    tr.poll();
    let mut open = true;
    let mut again = false;
    egui::Window::new(tl!("Translation")).id(id).open(&mut open).default_size([460.0, 360.0]).resizable(true).collapsible(true).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(tl!("Translate into"));
            let current = LANGUAGES.get(tr.target).map_or("", |(_, name)| name);
            egui::ComboBox::from_id_salt(id.with("lang")).selected_text(current).show_ui(ui, |ui| {
                for (i, (_, name)) in LANGUAGES.iter().enumerate() {
                    if ui.selectable_value(&mut tr.target, i, *name).changed() {
                        again = true;
                        if let Some((code, _)) = LANGUAGES.get(i) {
                            ui.ctx().data_mut(|d| d.insert_persisted(target_id(), (*code).to_owned()));
                        }
                    }
                }
            });
            if ui.add_enabled(!matches!(tr.state, State::Busy(_)), egui::Button::new(tl!("Translate again"))).clicked() {
                again = true;
            }
        });
        if tr.truncated {
            ui.weak(tl!("The selection was long: only its beginning was translated."));
        }
        ui.separator();
        match &tr.state {
            State::Idle => {
                ui.weak(tl!("Select some text and choose Translate."));
            }
            State::Busy(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tl!("Translating…"));
                });
            }
            State::Failed(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            State::Done(done) => {
                if let Some(lang) = &done.detected {
                    ui.weak(format!("{} {lang}", tl!("Detected language:")));
                }
                egui::ScrollArea::vertical().id_salt(id.with("text")).max_height((ui.available_height() - 36.0).max(80.0)).show(ui, |ui| {
                    // A read-only TextEdit, so the reader can select part of the translation.
                    ui.add(egui::TextEdit::multiline(&mut done.text.as_str()).desired_width(f32::INFINITY));
                });
                if ui.button(tl!("Copy translation")).clicked() {
                    ui.ctx().copy_text(done.text.clone());
                }
            }
        }
        ui.collapsing(tl!("Original text"), |ui| {
            egui::ScrollArea::vertical().id_salt(id.with("source")).max_height(160.0).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut tr.source.as_str()).desired_width(f32::INFINITY));
            });
        });
    });
    tr.open = open;
    if !open {
        // Closing drops the receiver; a request still running finishes and is discarded.
        tr.state = State::Idle;
    } else if again {
        tr.request(ctx);
    }
}

/// At most `max` characters of `text`, cut at a char boundary, and whether anything was cut.
fn cap(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (text.get(..at).unwrap_or_default().to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// Rejoin the lines of a selection into paragraphs, so the translation doesn't break where the
/// page's lines did: a blank line ends a paragraph, a word hyphenated across lines is joined, and
/// every other line break becomes a space.
fn unwrap_lines(text: &str) -> String {
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

/// Read Google's reply: `[[["translated","original",…],…],null,"en",…]`.
fn parse(body: &str) -> Reply {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| tl!("The translation service sent an unreadable reply."))?;
    let segments = value.get(0).and_then(|v| v.as_array()).ok_or_else(|| tl!("The translation service sent an unreadable reply."))?;
    let text: String = segments.iter().filter_map(|s| s.get(0).and_then(|t| t.as_str())).collect();
    let detected = value.get(2).and_then(|v| v.as_str()).map(str::to_owned);
    Ok(Translated { text, detected })
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn(ctx: egui::Context, text: String, target: &str) -> Result<Receiver<Reply>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let target = target.to_owned();
    let work = move || {
        let reply = translate(&text, &target);
        // The window may have closed and dropped the receiver: nothing left to tell.
        let _ = tx.send(reply);
        ctx.request_repaint();
    };
    std::thread::Builder::new().name("pdfcraft-translate".into()).spawn(work).map_err(|e| e.to_string())?;
    Ok(rx)
}

#[cfg(target_arch = "wasm32")]
fn spawn(_ctx: egui::Context, _text: String, _target: &str) -> Result<Receiver<Reply>, String> {
    Err(tl!("Translation is not available in the web version.").to_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn translate(text: &str, target: &str) -> Reply {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).build().into();
    let url = format!("https://translate.googleapis.com/translate_a/single?client=gtx&sl=auto&tl={target}&dt=t");
    let mut text_out = String::new();
    let mut detected = None;
    for piece in chunks(text, CHUNK_CHARS) {
        let mut response =
            agent.post(&url).send_form([("q", piece)]).map_err(|e| format!("{} {e}", tl!("Could not reach the translation service:")))?;
        let body = response.body_mut().read_to_string().map_err(|e| format!("{} {e}", tl!("Could not reach the translation service:")))?;
        let part = parse(&body)?;
        text_out.push_str(&part.text);
        detected = detected.or(part.detected);
    }
    Ok(Translated { text: text_out, detected })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_rebuild_the_text_and_respect_the_limit() {
        let text = "Prima riga.\nSeconda frase. Terza parola àèìòù 日本語 fine\n".repeat(40);
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
        assert_eq!(unwrap_lines("-\nà"), "à");
    }

    #[test]
    fn cap_cuts_at_char_boundaries() {
        assert_eq!(cap("àèì", 2), ("àè".to_owned(), true));
        assert_eq!(cap("àèì", 3), ("àèì".to_owned(), false));
        assert_eq!(cap("", 0), (String::new(), false));
    }

    #[test]
    fn parse_reads_segments_and_rejects_garbage() {
        let body = r#"[[["Ciao mondo. ","Hello world. ",null,null,10],["Seconda.","Second.",null]],null,"en",null]"#;
        let Ok(done) = parse(body) else { panic!("valid reply") };
        assert_eq!(done.text, "Ciao mondo. Seconda.");
        assert_eq!(done.detected.as_deref(), Some("en"));
        for bad in ["", "null", "{}", "[1]", "[[[1]]", "<html>"] {
            assert!(parse(bad).is_err() || parse(bad).is_ok_and(|d| d.text.is_empty()), "{bad}");
        }
    }
}
