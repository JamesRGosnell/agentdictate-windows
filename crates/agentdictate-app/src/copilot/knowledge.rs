//! What the copilot knows about the user beyond the conversation: documents
//! kept in the knowledge folder and notes from earlier meetings.
//!
//! Every answer searches this material, so replies about the user's own
//! business, prices, and history come from the user's own words. A small
//! document set is given whole to every answer; larger sets and meeting notes
//! are ranked against the question with BM25 and trimmed to a budget.

use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use agentdictate_core::KnowledgeSummary;

/// Formats read as plain text.
const TEXT_EXTENSIONS: [&str; 6] = ["md", "markdown", "txt", "text", "csv", "tsv"];
/// Formats people keep documents in that cannot be read yet.
const UNREADABLE_EXTENSIONS: [&str; 11] = [
    "pdf", "doc", "rtf", "odt", "ppt", "pptx", "key", "xls", "xlsx", "pages", "numbers",
];
const CHUNK_CHARS: usize = 1_200;
/// Documents this small in total are given whole to every answer.
const WHOLE_DOCUMENTS_CHARS: usize = 16_000;
/// Document excerpts added to one answer when the set is larger.
const DOCUMENT_BUDGET_CHARS: usize = 8_000;
/// Excerpts from earlier meetings added to one answer.
const MEETING_BUDGET_CHARS: usize = 3_000;
/// A meeting excerpt must share this many distinct words with the question.
const MEETING_MIN_SHARED_TERMS: usize = 2;
/// Excerpts scoring below this share of the best match are left out.
const RELEVANCE_FLOOR: f32 = 0.25;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_FILES: usize = 400;
const MAX_DEPTH: usize = 4;
const MAX_UNREADABLE_NAMES: usize = 8;

const STOP_WORDS: [&str; 64] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "could", "did", "do", "does",
    "for", "from", "had", "has", "have", "how", "if", "in", "into", "is", "it", "its", "just",
    "like", "me", "my", "no", "not", "of", "on", "or", "our", "so", "that", "the", "their", "them",
    "then", "there", "they", "this", "to", "up", "us", "was", "we", "were", "what", "when",
    "where", "which", "who", "why", "will", "with", "would", "you", "your", "yeah", "um",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Document,
    Meeting,
}

struct Chunk {
    source: String,
    origin: Origin,
    text: String,
    terms: HashMap<String, u32>,
    length: usize,
}

/// The searchable material, loaded once and replaced when files change.
#[derive(Default)]
pub(crate) struct Knowledge {
    chunks: Vec<Chunk>,
    document_chars: usize,
    document_frequency: HashMap<String, usize>,
    average_length: f32,
    summary: KnowledgeSummary,
}

impl Knowledge {
    /// Reads the knowledge folder and, when given, the meeting notes folder.
    /// Missing folders are simply empty.
    pub(crate) fn load(documents: &Path, meetings: Option<&Path>) -> Self {
        let mut knowledge = Self::default();
        let mut unreadable = Vec::new();
        for path in files(documents) {
            match read_document(&path) {
                Some(text) if !text.trim().is_empty() => {
                    let source = relative_name(documents, &path);
                    knowledge.summary.documents += 1;
                    knowledge.document_chars += text.len();
                    knowledge.add(&source, Origin::Document, &text);
                }
                Some(_) => {}
                None if is_unreadable_document(&path) => {
                    unreadable.push(relative_name(documents, &path));
                }
                None => {}
            }
        }
        if let Some(meetings) = meetings {
            for path in files(meetings) {
                if extension(&path).as_deref() != Some("md") {
                    continue;
                }
                if let Some(text) = read_document(&path) {
                    let title = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    knowledge.summary.meetings += 1;
                    knowledge.add(&format!("Meeting {title}"), Origin::Meeting, &text);
                }
            }
        }
        unreadable.sort();
        unreadable.truncate(MAX_UNREADABLE_NAMES);
        knowledge.summary.unreadable = unreadable;
        knowledge.index();
        knowledge
    }

    pub(crate) fn summary(&self) -> KnowledgeSummary {
        self.summary.clone()
    }

    fn add(&mut self, source: &str, origin: Origin, text: &str) {
        self.summary.words += text.split_whitespace().count();
        for piece in chunk_text(text) {
            let tokens = tokens(&piece);
            let mut terms = HashMap::new();
            for token in &tokens {
                *terms.entry(token.clone()).or_insert(0) += 1;
            }
            self.chunks.push(Chunk {
                source: source.to_owned(),
                origin,
                text: piece,
                terms,
                length: tokens.len().max(1),
            });
        }
    }

    fn index(&mut self) {
        self.document_frequency.clear();
        for chunk in &self.chunks {
            for term in chunk.terms.keys() {
                *self.document_frequency.entry(term.clone()).or_insert(0) += 1;
            }
        }
        let total: usize = self.chunks.iter().map(|chunk| chunk.length).sum();
        self.average_length = total as f32 / self.chunks.len().max(1) as f32;
    }

    /// Excerpts for answering `focus`, the question at hand, with `context`
    /// (the surrounding conversation) weighing in less. Each excerpt is
    /// labeled with its source; empty when nothing applies.
    pub(crate) fn excerpts(&self, focus: &str, context: &str) -> String {
        let mut query: HashMap<String, f32> = HashMap::new();
        for token in tokens(context) {
            *query.entry(token).or_insert(0.0) += 0.4;
        }
        for token in tokens(focus) {
            *query.entry(token).or_insert(0.0) += 1.0;
        }
        let mut picked: Vec<&Chunk> = Vec::new();
        if self.document_chars <= WHOLE_DOCUMENTS_CHARS {
            picked.extend(
                self.chunks
                    .iter()
                    .filter(|chunk| chunk.origin == Origin::Document),
            );
        } else {
            picked.extend(self.best(&query, Origin::Document, DOCUMENT_BUDGET_CHARS, 1));
        }
        picked.extend(self.best(
            &query,
            Origin::Meeting,
            MEETING_BUDGET_CHARS,
            MEETING_MIN_SHARED_TERMS,
        ));
        let mut rendered = String::new();
        let mut last_source = "";
        for chunk in picked {
            if chunk.source != last_source {
                if !rendered.is_empty() {
                    rendered.push_str("\n\n");
                }
                rendered.push('[');
                rendered.push_str(&chunk.source);
                rendered.push_str("]\n");
                last_source = &chunk.source;
            } else {
                rendered.push_str("\n\u{2026}\n");
            }
            rendered.push_str(chunk.text.trim());
        }
        rendered
    }

    /// The highest-scoring chunks of one origin that fit the budget, grouped
    /// by source in ranked order.
    fn best(
        &self,
        query: &HashMap<String, f32>,
        origin: Origin,
        budget: usize,
        min_shared: usize,
    ) -> Vec<&Chunk> {
        let total = self.chunks.len() as f32;
        let mut scored: Vec<(f32, usize)> = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| chunk.origin == origin)
            .filter_map(|(index, chunk)| {
                let mut shared = 0;
                let mut score = 0.0;
                for (term, weight) in query {
                    let Some(&frequency) = chunk.terms.get(term) else {
                        continue;
                    };
                    shared += 1;
                    let containing = self.document_frequency.get(term).copied().unwrap_or(0) as f32;
                    let idf = (1.0 + (total - containing + 0.5) / (containing + 0.5)).ln();
                    let frequency = frequency as f32;
                    let norm = 1.2 * (0.25 + 0.75 * chunk.length as f32 / self.average_length);
                    score += weight * idf * frequency * 2.2 / (frequency + norm);
                }
                (shared >= min_shared && score > 0.0).then_some((score, index))
            })
            .collect();
        scored.sort_by(|left, right| right.0.total_cmp(&left.0));
        // Weak matches would only crowd out the strong ones.
        let floor = scored.first().map_or(0.0, |best| best.0 * RELEVANCE_FLOOR);
        scored.retain(|(score, _)| *score >= floor);
        let mut used = 0;
        let mut chosen: Vec<usize> = Vec::new();
        for (_, index) in scored {
            let length = self.chunks[index].text.len();
            if used + length > budget {
                continue;
            }
            used += length;
            chosen.push(index);
        }
        // Keep each source's excerpts together, in reading order.
        let mut order: Vec<&str> = Vec::new();
        for &index in &chosen {
            let source = self.chunks[index].source.as_str();
            if !order.contains(&source) {
                order.push(source);
            }
        }
        chosen.sort_by_key(|&index| {
            let source = self.chunks[index].source.as_str();
            (order.iter().position(|seen| *seen == source), index)
        });
        chosen
            .into_iter()
            .map(|index| &self.chunks[index])
            .collect()
    }
}

/// Changes whenever a file in the folders is added, removed, or edited.
pub(crate) fn fingerprint(folders: &[&Path]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for folder in folders {
        for path in files(folder) {
            path.hash(&mut hasher);
            if let Ok(metadata) = std::fs::metadata(&path) {
                metadata.len().hash(&mut hasher);
                if let Ok(modified) = metadata.modified() {
                    modified.hash(&mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

/// Files under `root`, skipping hidden files and office lock files.
fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0)];
    while let Some((folder, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with('.') || name.starts_with("~$") {
                continue;
            }
            if path.is_dir() {
                if depth < MAX_DEPTH {
                    pending.push((path, depth + 1));
                }
            } else if found.len() < MAX_FILES {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
}

fn is_unreadable_document(path: &Path) -> bool {
    extension(path).is_some_and(|extension| UNREADABLE_EXTENSIONS.contains(&extension.as_str()))
}

fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Text of a supported file; `None` for other formats or unreadable files.
fn read_document(path: &Path) -> Option<String> {
    let extension = extension(path)?;
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    if TEXT_EXTENSIONS.contains(&extension.as_str()) {
        let bytes = std::fs::read(path).ok()?;
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    if extension == "docx" {
        let bytes = std::fs::read(path).ok()?;
        let xml = zip_entry(&bytes, "word/document.xml")?;
        return Some(docx_text(&String::from_utf8_lossy(&xml)));
    }
    None
}

/// One file from a ZIP archive, such as the body of a Word document.
fn zip_entry(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    let u16_at = |offset: usize| -> Option<usize> {
        Some(u16::from_le_bytes(archive.get(offset..offset + 2)?.try_into().ok()?) as usize)
    };
    let u32_at = |offset: usize| -> Option<usize> {
        Some(u32::from_le_bytes(archive.get(offset..offset + 4)?.try_into().ok()?) as usize)
    };
    // The end-of-central-directory record sits within the last 64 KiB.
    let search_from = archive.len().saturating_sub(22 + 65_535);
    let end = (search_from..archive.len().saturating_sub(21))
        .rev()
        .find(|&offset| u32_at(offset) == Some(0x0605_4b50))?;
    let entries = u16_at(end + 10)?;
    let mut offset = u32_at(end + 16)?;
    for _ in 0..entries {
        if u32_at(offset)? != 0x0201_4b50 {
            return None;
        }
        let method = u16_at(offset + 10)?;
        let compressed = u32_at(offset + 20)?;
        let size = u32_at(offset + 24)?;
        let name_length = u16_at(offset + 28)?;
        let extra_length = u16_at(offset + 30)?;
        let comment_length = u16_at(offset + 32)?;
        let local = u32_at(offset + 42)?;
        let entry_name = archive.get(offset + 46..offset + 46 + name_length)?;
        if entry_name == name.as_bytes() {
            if u32_at(local)? != 0x0403_4b50 {
                return None;
            }
            let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let data = archive.get(start..start + compressed)?;
            return match method {
                0 => Some(data.to_vec()),
                8 => {
                    miniz_oxide::inflate::decompress_to_vec_with_limit(data, size.max(1) * 2 + 1024)
                        .ok()
                }
                _ => None,
            };
        }
        offset += 46 + name_length + extra_length + comment_length;
    }
    None
}

/// Paragraph text from WordprocessingML.
fn docx_text(xml: &str) -> String {
    let mut text = String::new();
    let mut in_text = false;
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        if in_text {
            text.push_str(&decode_entities(&rest[..open]));
        }
        let Some(length) = rest[open..].find('>') else {
            break;
        };
        let tag = &rest[open + 1..open + length];
        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|character: char| character.is_whitespace() || character == '/')
            .next()
            .unwrap_or_default();
        match name {
            "w:t" => in_text = !closing && !self_closing,
            "w:p" if closing || self_closing => text.push('\n'),
            "w:tab" => text.push('\t'),
            "w:br" | "w:cr" => text.push('\n'),
            _ => {}
        }
        rest = &rest[open + length + 1..];
    }
    text
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find(';').filter(|&end| end <= 10) else {
            decoded.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..end];
        let character = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match character {
            Some(character) => {
                decoded.push(character);
                rest = &after[end + 1..];
            }
            None => {
                decoded.push('&');
                rest = &after[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

/// Splits text into excerpts of about [`CHUNK_CHARS`] at paragraph breaks,
/// carrying the nearest Markdown heading into each excerpt it covers.
fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut heading = String::new();
    let paragraphs = text
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty());
    for paragraph in paragraphs {
        if paragraph.starts_with('#') {
            heading = paragraph.lines().next().unwrap_or_default().to_owned();
        }
        for piece in split_long(paragraph) {
            // A heading alone never becomes its own excerpt.
            if !current.is_empty()
                && current != heading
                && current.len() + piece.len() + 2 > CHUNK_CHARS
            {
                chunks.push(std::mem::take(&mut current));
            }
            if current.is_empty() && !heading.is_empty() && !piece.starts_with('#') {
                current.push_str(&heading);
                current.push('\n');
            } else if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(piece);
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Breaks a paragraph longer than an excerpt at whitespace.
fn split_long(paragraph: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = paragraph;
    while rest.len() > CHUNK_CHARS {
        let mut cut = CHUNK_CHARS;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let cut = rest[..cut]
            .rfind(char::is_whitespace)
            .filter(|&space| space > CHUNK_CHARS / 2)
            .unwrap_or(cut);
        pieces.push(rest[..cut].trim());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

/// Lowercase word stems worth matching, without stop words.
fn tokens(text: &str) -> Vec<String> {
    let stop: HashSet<&str> = STOP_WORDS.into_iter().collect();
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.len() > 1 && !stop.contains(word))
        .map(stem)
        .collect()
}

/// A light stem, so "certified" matches "certification" and "prices" matches
/// "price": common endings are dropped and long words are cut to a prefix.
fn stem(word: &str) -> String {
    let word = ["ing", "ed", "s"]
        .iter()
        .find_map(|suffix| word.strip_suffix(suffix).filter(|stem| stem.len() >= 4))
        .unwrap_or(word);
    word.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(folder: &Path, name: &str, text: &str) {
        let path = folder.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    /// A minimal stored (uncompressed) Word document.
    fn docx(body: &str) -> Vec<u8> {
        let name = b"word/document.xml";
        let data = body.as_bytes();
        let mut archive = Vec::new();
        archive.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        archive.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());
        archive.extend_from_slice(&(name.len() as u16).to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive.extend_from_slice(name);
        archive.extend_from_slice(data);
        let central = archive.len();
        archive.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        archive.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());
        archive.extend_from_slice(&(name.len() as u16).to_le_bytes());
        archive.extend_from_slice(&[0; 12]);
        archive.extend_from_slice(&0_u32.to_le_bytes());
        archive.extend_from_slice(name);
        let central_size = archive.len() - central;
        archive.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        archive.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        archive.extend_from_slice(&(central_size as u32).to_le_bytes());
        archive.extend_from_slice(&(central as u32).to_le_bytes());
        archive.extend_from_slice(&0_u16.to_le_bytes());
        archive
    }

    #[test]
    fn small_document_sets_are_given_whole_with_their_sources() {
        let folder = tempfile::tempdir().unwrap();
        write(
            folder.path(),
            "pricing.md",
            "# Pricing\n\nGrowth plan: $49 per seat per month.",
        );
        write(
            folder.path(),
            "team/bio.txt",
            "James founded LeadLord in 2024.",
        );
        write(folder.path(), "deck.pdf", "%PDF-1.7");
        write(folder.path(), "~$pricing.docx", "lock");
        let knowledge = Knowledge::load(folder.path(), None);
        let summary = knowledge.summary();
        assert_eq!((summary.documents, summary.meetings), (2, 0));
        assert_eq!(summary.unreadable, vec!["deck.pdf"]);
        let excerpts = knowledge.excerpts("What's the weather?", "");
        assert!(excerpts.contains("[pricing.md]\n# Pricing\n\nGrowth plan: $49 per seat"));
        assert!(excerpts.contains("[team/bio.txt]\nJames founded LeadLord"));
    }

    #[test]
    fn large_document_sets_and_meetings_are_searched() {
        let folder = tempfile::tempdir().unwrap();
        let meetings = tempfile::tempdir().unwrap();
        for index in 0..30 {
            write(
                folder.path(),
                &format!("notes-{index:02}.md"),
                &format!(
                    "Unrelated filler about topic {index}. {}",
                    "Lorem ipsum dolor. ".repeat(40)
                ),
            );
        }
        write(
            folder.path(),
            "security.md",
            "# Security\n\nWe are SOC 2 Type II certified and encrypt data at rest.",
        );
        write(
            meetings.path(),
            "2026-10-01 0900 Acme renewal.md",
            "Acme asked about SOC 2 certification and data retention.",
        );
        write(
            meetings.path(),
            "2026-09-01 0900 Hiring sync.md",
            "Discussed hiring a designer.",
        );
        let knowledge = Knowledge::load(folder.path(), Some(meetings.path()));
        assert_eq!(knowledge.summary().meetings, 2);
        let excerpts = knowledge.excerpts("Are you SOC 2 certified?", "We care about security.");
        assert!(excerpts.contains("[security.md]"));
        assert!(excerpts.contains("[Meeting 2026-10-01 0900 Acme renewal]"));
        assert!(!excerpts.contains("Hiring sync"));
        assert!(!excerpts.contains("notes-00.md"));
        assert!(excerpts.len() < DOCUMENT_BUDGET_CHARS + MEETING_BUDGET_CHARS + 200);
    }

    #[test]
    fn word_documents_are_read() {
        let folder = tempfile::tempdir().unwrap();
        let body = r#"<w:document><w:body><w:p><w:r><w:t>Case study:</w:t></w:r><w:r><w:t xml:space="preserve"> Acme &amp; Co</w:t></w:r></w:p><w:p><w:r><w:t>Saved 40%</w:t></w:r></w:p></w:body></w:document>"#;
        std::fs::write(folder.path().join("case.docx"), docx(body)).unwrap();
        let knowledge = Knowledge::load(folder.path(), None);
        assert_eq!(knowledge.summary().documents, 1);
        assert!(
            knowledge
                .excerpts("", "")
                .contains("[case.docx]\nCase study: Acme & Co\nSaved 40%")
        );
    }

    #[test]
    fn edits_change_the_fingerprint() {
        let folder = tempfile::tempdir().unwrap();
        let empty = fingerprint(&[folder.path()]);
        write(folder.path(), "faq.md", "Q: Refunds? A: 30 days.");
        let one = fingerprint(&[folder.path()]);
        assert_ne!(empty, one);
        assert_eq!(one, fingerprint(&[folder.path()]));
    }

    #[test]
    fn long_text_is_split_into_excerpts_under_their_heading() {
        let text = format!("# Offer\n\n{}", "Word ".repeat(600));
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= CHUNK_CHARS + 20));
        assert!(chunks.iter().all(|chunk| chunk.starts_with("# Offer")));
        assert_eq!(
            decode_entities("a &lt;b&gt; &#65;&#x42; & c"),
            "a <b> AB & c"
        );
        assert_eq!(
            tokens("Certified certification prices"),
            ["certifi", "certifi", "price"]
        );
    }
}
