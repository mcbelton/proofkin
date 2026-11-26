//! A Word document's text as Markdown, for a spec imported from SharePoint or OneDrive
//! (`proofkin spec import --from`). Only `word/document.xml` is read, within a size limit:
//! the body's titles and headings, list items, tables and paragraphs. Headers, footers,
//! comments and images are left out.

use std::borrow::Cow;
use std::io::{Cursor, Read};
use std::ops::Deref;

use anyhow::{Context, Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// The most `word/document.xml` may hold uncompressed.
pub const MAX_DOCUMENT_XML: u64 = 8 * 1024 * 1024;

const DOCUMENT_XML: &str = "word/document.xml";

/// Elements whose content is left out: images and text boxes (`drawing`, `pict`, `object`),
/// the fallback copy of alternate content, and text moved elsewhere by a tracked change.
const SKIPPED: [&str; 5] = ["drawing", "pict", "object", "Fallback", "moveFrom"];

/// The text of the Word (`.docx`) document `bytes` as Markdown.
pub fn to_markdown(bytes: &[u8]) -> Result<String> {
    markdown_of(&document_xml(bytes)?)
}

/// The Word document's `word/document.xml`, refused if it holds more than
/// [`MAX_DOCUMENT_XML`] uncompressed. No other entry is read.
fn document_xml(bytes: &[u8]) -> Result<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context(
        "could not open the Word document: it is not a zip file (an encrypted document can't \
         be read)",
    )?;
    let entry = archive
        .by_name(DOCUMENT_XML)
        .context("could not read the Word document: it has no word/document.xml")?;
    let too_big = || {
        anyhow!(
            "the Word document's text is too big: word/document.xml may hold at most {} MB",
            MAX_DOCUMENT_XML / (1024 * 1024)
        )
    };
    if entry.size() > MAX_DOCUMENT_XML {
        return Err(too_big());
    }
    let mut xml = Vec::new();
    entry
        .take(MAX_DOCUMENT_XML + 1)
        .read_to_end(&mut xml)
        .context("could not read the Word document's word/document.xml")?;
    if xml.len() as u64 > MAX_DOCUMENT_XML {
        return Err(too_big());
    }
    String::from_utf8(xml).context("the Word document's word/document.xml is not UTF-8")
}

/// The Markdown for the WordprocessingML `xml`.
fn markdown_of(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    let mut body = Body::default();
    loop {
        match reader
            .read_event()
            .context("the Word document's word/document.xml is not valid XML")?
        {
            Event::Start(e) => body.open(&e),
            Event::Empty(e) => {
                body.open(&e);
                body.close(e.local_name().as_ref());
            }
            Event::End(e) => body.close(e.local_name().as_ref()),
            Event::Text(t) => body.text(&unescape(&lossy(&t))),
            Event::GeneralRef(r) => {
                let name = lossy(&r);
                let name = name.trim_start_matches('&').trim_end_matches(';');
                match entity(name) {
                    Some(c) => body.text(c.encode_utf8(&mut [0; 4])),
                    None => body.text(&format!("&{name};")),
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(body.markdown())
}

/// The content of an event (bytes, or text in newer versions of quick-xml) as text.
fn lossy<T>(event: &T) -> Cow<'_, str>
where
    T: Deref,
    T::Target: AsRef<[u8]>,
{
    String::from_utf8_lossy(event.deref().as_ref())
}

/// A finished piece of the Markdown.
enum Block {
    /// A paragraph, a heading or a table.
    Text(String),
    /// A list item; items in a row are not separated by a blank line.
    Item(String),
}

#[derive(Default)]
struct Paragraph {
    style: Option<String>,
    list: bool,
    level: usize,
    text: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    /// The paragraphs of the cell being read.
    cell: Option<Vec<String>>,
}

/// The document's body, built while its XML is read.
#[derive(Default)]
struct Body {
    blocks: Vec<Block>,
    /// Open paragraphs, innermost last.
    paragraphs: Vec<Paragraph>,
    /// Open tables, innermost last.
    tables: Vec<Table>,
    /// How deep inside a left-out element the reader is.
    skip: usize,
    /// How many runs (`w:r`) are open.
    runs: usize,
    /// Inside a run's text (`w:t`).
    in_text: bool,
}

impl Body {
    fn open(&mut self, e: &BytesStart) {
        let name = e.local_name();
        let name: &str = name.as_ref();
        if self.skip > 0 || SKIPPED.contains(&name) {
            self.skip += 1;
            return;
        }
        match name {
            "p" => self.paragraphs.push(Paragraph::default()),
            "pStyle" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.style = val(e);
                }
            }
            "numPr" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.list = true;
                }
            }
            "ilvl" => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.level = val(e).and_then(|v| v.parse().ok()).unwrap_or(0).min(8);
                }
            }
            // Numbering 0 takes away the numbering a style gives.
            "numId" if val(e).as_deref() == Some("0") => {
                if let Some(p) = self.paragraphs.last_mut() {
                    p.list = false;
                }
            }
            "tbl" => self.tables.push(Table::default()),
            "tr" => {
                if let Some(table) = self.tables.last_mut() {
                    table.rows.push(Vec::new());
                }
            }
            "tc" => {
                if let Some(table) = self.tables.last_mut() {
                    table.cell = Some(Vec::new());
                }
            }
            "r" => self.runs += 1,
            "t" => self.in_text = true,
            // A tab in the paragraph's properties is a tab stop, not text.
            "tab" if self.runs > 0 => self.push("\t"),
            "br" | "cr" if self.runs > 0 => self.push("\n"),
            "noBreakHyphen" if self.runs > 0 => self.push("-"),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        match name {
            "p" => {
                if let Some(paragraph) = self.paragraphs.pop() {
                    self.paragraph(paragraph);
                }
            }
            "r" => self.runs = self.runs.saturating_sub(1),
            "t" => self.in_text = false,
            "tc" => self.cell(),
            "tbl" => self.table(),
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.skip == 0 && self.in_text {
            self.push(text);
        }
    }

    fn push(&mut self, text: &str) {
        if let Some(p) = self.paragraphs.last_mut() {
            p.text.push_str(text);
        }
    }

    fn paragraph(&mut self, paragraph: Paragraph) {
        let text = paragraph.text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.cell.as_mut()) {
            cell.push(one_line(text));
            return;
        }
        let style = paragraph.style.as_deref().unwrap_or("");
        let block = if let Some(level) = heading_level(style) {
            Block::Text(format!("{} {}", "#".repeat(level), one_line(text)))
        } else if paragraph.list || is_list_style(style) {
            Block::Item(format!("{}- {text}", "  ".repeat(paragraph.level)))
        } else {
            Block::Text(text.to_owned())
        };
        self.blocks.push(block);
    }

    fn cell(&mut self) {
        let Some(table) = self.tables.last_mut() else {
            return;
        };
        let Some(paragraphs) = table.cell.take() else {
            return;
        };
        let text = paragraphs.join(" ");
        match table.rows.last_mut() {
            Some(row) => row.push(text),
            None => table.rows.push(vec![text]),
        }
    }

    fn table(&mut self) {
        let Some(table) = self.tables.pop() else {
            return;
        };
        let rows: Vec<Vec<String>> = table
            .rows
            .into_iter()
            .filter(|row| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        if rows.is_empty() {
            return;
        }
        // Markdown has no tables in tables: one in a cell becomes that cell's text.
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.cell.as_mut()) {
            let text: Vec<String> = rows.iter().map(|row| row.join(" ")).collect();
            cell.push(text.join(" "));
            return;
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let line = |row: &[String]| {
            let cells: Vec<String> = (0..width)
                .map(|i| row.get(i).map_or(String::new(), |c| c.replace('|', "\\|")))
                .collect();
            format!("| {} |", cells.join(" | "))
        };
        let mut lines = vec![line(&rows[0]), format!("|{}", " --- |".repeat(width))];
        for row in &rows[1..] {
            lines.push(line(row));
        }
        self.blocks.push(Block::Text(lines.join("\n")));
    }

    fn markdown(&self) -> String {
        let mut out = String::new();
        let mut after_item = false;
        for block in &self.blocks {
            let (text, item) = match block {
                Block::Text(text) => (text, false),
                Block::Item(text) => (text, true),
            };
            if !out.is_empty() {
                out.push_str(if item && after_item { "\n" } else { "\n\n" });
            }
            out.push_str(text);
            after_item = item;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

/// The element's `val` attribute (`w:val`).
fn val(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == "val")
        .map(|a| unescape(&a.value))
}

/// The Markdown heading level of the paragraph style `style`: 1 for `Title` and `Heading1`,
/// up to 6 for `Heading6`.
fn heading_level(style: &str) -> Option<usize> {
    let style = style.to_ascii_lowercase();
    if style == "title" {
        return Some(1);
    }
    let level: usize = style.strip_prefix("heading")?.parse().ok()?;
    (1..=6).contains(&level).then_some(level)
}

/// Whether the paragraph style `style` is one of Word's bullet or numbered list styles.
fn is_list_style(style: &str) -> bool {
    let style = style.to_ascii_lowercase();
    style.starts_with("listbullet") || style.starts_with("listnumber")
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with XML's character references and predefined entities replaced; any other
/// reference is kept as it is.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let reference = rest
            .find(';')
            .and_then(|end| entity(&rest[1..end]).map(|c| (c, end)));
        match reference {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The character the reference `name` (between `&` and `;`) stands for.
fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    /// A Word document whose body is `body` (WordprocessingML), with a header and comments
    /// whose text is left out.
    pub(crate) fn docx(body: &str) -> Vec<u8> {
        let document = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <w:document xmlns:w=\"{W}\"><w:body>{body}<w:sectPr>\
             <w:headerReference w:type=\"default\"/></w:sectPr></w:body></w:document>"
        );
        let header = format!("<w:hdr xmlns:w=\"{W}\">{}</w:hdr>", para("", "HEADER-TEXT"));
        let comments = format!(
            "<w:comments xmlns:w=\"{W}\"><w:comment w:id=\"0\">{}</w:comment></w:comments>",
            para("", "COMMENT-TEXT")
        );
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("[Content_Types].xml", "<Types/>"),
            ("word/document.xml", document.as_str()),
            ("word/header1.xml", header.as_str()),
            ("word/comments.xml", comments.as_str()),
        ] {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(name, options).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// A paragraph of style `style` (none for `""`) holding `text`.
    pub(crate) fn para(style: &str, text: &str) -> String {
        let style = if style.is_empty() {
            String::new()
        } else {
            format!("<w:pPr><w:pStyle w:val=\"{style}\"/></w:pPr>")
        };
        format!("<w:p>{style}<w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>")
    }

    /// A numbered paragraph at list level `level`, with a tab stop, which is no text.
    pub(crate) fn item(level: usize, text: &str) -> String {
        let numbering =
            format!("<w:numPr><w:ilvl w:val=\"{level}\"/><w:numId w:val=\"3\"/></w:numPr>");
        let tabs = "<w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs>";
        format!(
            "<w:p><w:pPr><w:pStyle w:val=\"ListParagraph\"/>{numbering}{tabs}</w:pPr>\
             <w:r><w:t>{text}</w:t></w:r></w:p>"
        )
    }

    /// A table cell holding `paragraphs`.
    pub(crate) fn cell(paragraphs: &[&str]) -> String {
        let paragraphs: String = paragraphs.iter().map(|text| para("", text)).collect();
        format!("<w:tc><w:tcPr/>{paragraphs}</w:tc>")
    }

    #[test]
    fn word_documents_read_as_markdown() {
        let table = format!(
            "<w:tbl><w:tblPr/><w:tblGrid><w:gridCol/></w:tblGrid>\
             <w:tr>{}{}</w:tr><w:tr>{}{}</w:tr><w:tr>{}</w:tr></w:tbl>",
            cell(&["Role"]),
            cell(&["Can"]),
            cell(&["Owner"]),
            cell(&["read | write", "share"]),
            cell(&["Guest"]),
        );
        let body = [
            para("Title", "Photo albums"),
            para("heading2", "User stories"),
            // Runs with a tab, a line break and escaped characters.
            "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Name</w:t></w:r><w:r><w:tab/>\
             <w:t>Photos &amp; albums &#8211; &lt;all&gt;</w:t></w:r>\
             <w:r><w:br/><w:t>second line</w:t></w:r></w:p>"
                .to_owned(),
            item(0, "first item"),
            item(1, "nested item"),
            // An image with a text box, deleted text and a field's code are left out.
            "<w:p><w:r><w:drawing><w:txbxContent><w:p><w:r><w:t>IMAGE-TEXT</w:t></w:r></w:p>\
             </w:txbxContent></w:drawing></w:r></w:p>"
                .to_owned(),
            "<w:p><w:del><w:r><w:delText>DELETED</w:delText></w:r></w:del><w:ins><w:r>\
             <w:instrText> PAGE </w:instrText><w:t>Inserted</w:t></w:r></w:ins>\
             <w:commentReference w:id=\"0\"/></w:p><w:p/>"
                .to_owned(),
            table,
            // Numbering 0 is no list, and Markdown has no seventh heading level.
            "<w:p><w:pPr><w:numPr><w:ilvl w:val=\"0\"/><w:numId w:val=\"0\"/></w:numPr>\
             </w:pPr><w:r><w:t>Not a list</w:t></w:r></w:p>"
                .to_owned(),
            para("Heading7", "Deep"),
            para("ListBullet", "styled item"),
        ]
        .concat();
        let markdown = to_markdown(&docx(&body)).unwrap();
        assert_eq!(
            markdown,
            "# Photo albums\n\n\
             ## User stories\n\n\
             Name\tPhotos & albums \u{2013} <all>\nsecond line\n\n\
             - first item\n  - nested item\n\n\
             Inserted\n\n\
             | Role | Can |\n| --- | --- |\n| Owner | read \\| write share |\n| Guest |  |\n\n\
             Not a list\n\n\
             Deep\n\n\
             - styled item\n"
        );
        for left_out in ["HEADER", "COMMENT", "IMAGE", "DELETED", "PAGE"] {
            assert!(!markdown.contains(left_out), "{left_out}: {markdown}");
        }
    }

    #[test]
    fn oversized_or_broken_word_documents_are_refused() {
        // Compressed, the text is small; uncompressed it is over the limit.
        let big = para("", &"x".repeat(MAX_DOCUMENT_XML as usize));
        let err = to_markdown(&docx(&big)).unwrap_err().to_string();
        assert!(err.contains("too big"), "{err}");

        let err = to_markdown(b"PK not really").unwrap_err().to_string();
        assert!(err.contains("not a zip file"), "{err}");

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("word/other.xml", options).unwrap();
        let other = zip.finish().unwrap().into_inner();
        let err = to_markdown(&other).unwrap_err().to_string();
        assert!(err.contains("no word/document.xml"), "{err}");

        let err = markdown_of("<w:document><w:p>open</w:p><w:p")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid XML"), "{err}");
    }
}
