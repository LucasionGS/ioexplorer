//! Finding a GIF or image on the clipboard to offer for saving.
//!
//! What a copy puts on the clipboard depends on where it came from, and the
//! image itself is often the worst of it: "Copy image" in a Chromium browser
//! offers a still PNG of an animated GIF. The link to the original is usually
//! there too, so links are followed first and the image data used last:
//!
//! 1. a copied file (`text/uri-list`),
//! 2. the `<img src>` of copied HTML, which is what "Copy image" adds,
//! 3. a copied link, to an image or to a Tenor or Giphy page,
//! 4. the image data.

use std::time::Duration;

use gtk::{gdk, gio, glib, prelude::*};

use super::library::{ImageKind, Library};

/// Largest image downloaded or read from the clipboard.
const MAX_IMAGE_BYTES: u64 = 40 * 1024 * 1024;
/// Largest page read looking for its preview image.
const MAX_PAGE_BYTES: u64 = 2 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// An image found on the clipboard.
#[derive(Clone, Debug)]
pub struct Found {
    pub bytes: Vec<u8>,
    pub kind: ImageKind,
    /// The link it came from, kept so it can be inserted as that link.
    pub source: Option<String>,
}

/// What the clipboard holds, as far as the GIF tab is concerned.
#[derive(Clone, Debug)]
pub enum Detected {
    Nothing,
    /// An image that is not saved yet.
    New(Found),
    /// An image that is saved already, as this file.
    Saved(String),
}

/// Looks through the clipboard for an image. Links are downloaded, so this can
/// take a moment; it never blocks the main loop while it does.
pub async fn detect(clipboard: &gdk::Clipboard, library: &Library) -> Detected {
    let formats = clipboard.formats();
    let has = |mime: &str| formats.contain_mime_type(mime);

    let mut links: Vec<String> = Vec::new();

    if has("text/uri-list")
        && let Some(list) = read(clipboard, "text/uri-list", 64 * 1024).await
    {
        for uri in String::from_utf8_lossy(&list)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            if uri.starts_with("file://") {
                if let Some(found) = read_file(uri).await {
                    return judge(found, library);
                }
            } else if is_web_link(uri) {
                links.push(uri.to_string());
            }
        }
    }

    if has("text/html")
        && let Some(html) = read(clipboard, "text/html", MAX_PAGE_BYTES as usize).await
        && let Some(src) = img_src(&decode_text(&html))
    {
        links.push(src);
    }

    if let Ok(Some(text)) = clipboard.read_text_future().await {
        let text = text.trim();
        if is_web_link(text) && !text.contains(char::is_whitespace) {
            links.push(text.to_string());
        }
    }

    for link in links {
        if let Some(path) = library.find_source(&link) {
            return saved(&path);
        }
        let fetched = link.clone();
        let result = gio::spawn_blocking(move || fetch_image(&fetched))
            .await
            .unwrap_or_else(|_| Err("the download stopped".to_string()));
        match result {
            Ok((bytes, kind)) => {
                return judge(
                    Found {
                        bytes,
                        kind,
                        source: Some(link),
                    },
                    library,
                );
            }
            Err(error) => tracing::info!(%link, %error, "no image behind the copied link"),
        }
    }

    for kind in [
        ImageKind::Gif,
        ImageKind::Webp,
        ImageKind::Png,
        ImageKind::Jpeg,
    ] {
        if !has(kind.mime_type()) {
            continue;
        }
        if let Some(bytes) = read(clipboard, kind.mime_type(), MAX_IMAGE_BYTES as usize).await
            && let Some(kind) = ImageKind::sniff(&bytes)
        {
            return judge(
                Found {
                    bytes,
                    kind,
                    source: None,
                },
                library,
            );
        }
    }

    Detected::Nothing
}

fn judge(found: Found, library: &Library) -> Detected {
    match library.find_same(&found.bytes) {
        Some(path) => saved(&path),
        None => Detected::New(found),
    }
}

fn saved(path: &std::path::Path) -> Detected {
    Detected::Saved(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string(),
    )
}

async fn read(clipboard: &gdk::Clipboard, mime: &str, limit: usize) -> Option<Vec<u8>> {
    let (stream, _) = clipboard
        .read_future(&[mime], glib::Priority::DEFAULT)
        .await
        .ok()?;
    read_stream(&stream, limit).await
}

async fn read_stream(stream: &gio::InputStream, limit: usize) -> Option<Vec<u8>> {
    let mut contents = Vec::new();
    loop {
        let chunk = stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await
            .ok()?;
        if chunk.is_empty() {
            return Some(contents);
        }
        contents.extend_from_slice(&chunk);
        if contents.len() > limit {
            return None;
        }
    }
}

async fn read_file(uri: &str) -> Option<Found> {
    let file = gio::File::for_uri(uri);
    let path = file.path()?;
    let bytes = gio::spawn_blocking(move || {
        let size = std::fs::metadata(&path).ok()?.len();
        (size <= MAX_IMAGE_BYTES)
            .then(|| std::fs::read(&path).ok())
            .flatten()
    })
    .await
    .ok()??;
    let kind = ImageKind::sniff(&bytes)?;
    Some(Found {
        bytes,
        kind,
        source: None,
    })
}

fn is_web_link(text: &str) -> bool {
    text.starts_with("https://") || text.starts_with("http://")
}

/// Browsers put HTML on the clipboard as UTF-8, some as UTF-16 with a byte
/// order mark.
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// The first web `src` of an `<img>` in copied HTML.
fn img_src(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = lower[from..].find("<img") {
        let tag_start = from + start;
        let tag_end = lower[tag_start..]
            .find('>')
            .map_or(lower.len(), |end| tag_start + end);
        if let Some(src) = attribute(&html[tag_start..tag_end], "src")
            && is_web_link(&src)
        {
            return Some(src);
        }
        from = tag_end;
    }
    None
}

/// The value of `name` in one HTML tag, quoted either way, with the common
/// entity in URLs decoded.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        // A whole attribute name, not the end of another (`data-src`).
        let before = lower[..at].chars().next_back();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let value = match rest.chars().next()? {
            quote @ ('"' | '\'') => rest[1..].split(quote).next()?,
            _ => rest
                .split(|ch: char| ch.is_whitespace() || ch == '>')
                .next()?,
        };
        return Some(value.replace("&amp;", "&"));
    }
    None
}

/// The `og:image` of a page, when it is a GIF. That is how Tenor and Giphy
/// pages point at their GIF; other pages' preview images are not what was
/// meant by copying their link.
fn og_gif(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = lower[from..].find("<meta") {
        let tag_start = from + start;
        let tag_end = lower[tag_start..]
            .find('>')
            .map_or(lower.len(), |end| tag_start + end);
        let tag = &html[tag_start..tag_end];
        from = tag_end;
        let property = attribute(tag, "property").or_else(|| attribute(tag, "name"));
        if !property.is_some_and(|property| property.eq_ignore_ascii_case("og:image")) {
            continue;
        }
        if let Some(content) = attribute(tag, "content")
            && is_web_link(&content)
            && content
                .split(['?', '#'])
                .next()
                .is_some_and(|path| path.to_ascii_lowercase().ends_with(".gif"))
        {
            return Some(content);
        }
    }
    None
}

/// Downloads `url`, following a page to its GIF. Blocking — worker only.
fn fetch_image(url: &str) -> Result<(Vec<u8>, ImageKind), String> {
    let bytes = get(url, MAX_IMAGE_BYTES)?;
    if let Some(kind) = ImageKind::sniff(&bytes) {
        return Ok((bytes, kind));
    }
    let page = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PAGE_BYTES as usize)]);
    let gif = og_gif(&page).ok_or("the link is not an image")?;
    let bytes = get(&gif, MAX_IMAGE_BYTES)?;
    let kind = ImageKind::sniff(&bytes).ok_or("the page's GIF is not an image")?;
    Ok((bytes, kind))
}

fn get(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let response = ureq::get(url)
        .config()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(RESPONSE_TIMEOUT))
        .build()
        .call()
        .map_err(|error| error.to_string())?;
    response
        .into_body()
        .into_with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_images_point_at_their_source() {
        let html = r#"<meta charset="utf-8"><img alt="x" data-src="no" src="https://media.tenor.com/a/cat.gif?a=1&amp;b=2">"#;
        assert_eq!(
            img_src(html).as_deref(),
            Some("https://media.tenor.com/a/cat.gif?a=1&b=2")
        );
        assert_eq!(
            img_src("<IMG SRC='http://x.test/y.png'>").as_deref(),
            Some("http://x.test/y.png")
        );
        assert_eq!(img_src(r#"<img src="data:image/png;base64,AAAA">"#), None);
        assert_eq!(img_src("<p>no image</p>"), None);
    }

    #[test]
    fn pages_lead_to_their_gif_only() {
        let tenor = r#"<head><meta property="og:image" content="https://media1.tenor.com/m/x/cat.gif"></head>"#;
        assert_eq!(
            og_gif(tenor).as_deref(),
            Some("https://media1.tenor.com/m/x/cat.gif")
        );
        let article = r#"<meta property="og:image" content="https://news.test/cover.jpg">"#;
        assert_eq!(og_gif(article), None);
    }

    #[test]
    fn utf16_html_is_read() {
        let mut bytes = vec![0xff, 0xfe];
        for unit in "<img src=\"https://a.test/b.gif\">".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            img_src(&decode_text(&bytes)).as_deref(),
            Some("https://a.test/b.gif")
        );
    }
}
