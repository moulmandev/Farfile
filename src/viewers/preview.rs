//! The quick preview of one file: its first megabyte as text or as bytes,
//! or the whole of it as an image, read without downloading the file.

use std::sync::Arc;
use std::time::Duration;

use egui::{ColorImage, TextureHandle, TextureOptions};

use super::decode::{self, Decoder, Encoding, Row, TextDoc};
use super::task::{Spawner, Task};
use crate::format;
use crate::storage::{Chunk, Entry, RemotePath, Storage};

/// Read at first, and at each "load more".
pub const CHUNK: u64 = 1024 * 1024;
/// Past this much, the rest of a file is for the editor or a download.
pub const MAX_LOADED: u64 = 32 * 1024 * 1024;
/// Images heavier than this are not decoded.
pub const MAX_IMAGE: u64 = 40 * 1024 * 1024;
/// Images are scaled down to fit a texture this wide and high.
const TEXTURE_SIDE: u32 = 4096;
/// Wider or higher than this is refused before decoding.
const MAX_SIDE: u32 = 20_000;
/// Lines wider than this are wrapped from the start: minified files.
const WRAP_WIDER_THAN: usize = 2_000;
/// A pause before reading, so that going through files quickly only
/// reads the one stopped on.
const SETTLE: Duration = Duration::from_millis(120);

/// Where the previewed file was picked: Up and Down go through that list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Listing,
    Search,
}

pub struct TextBody {
    decoder: Decoder,
    pub doc: TextDoc,
    /// The JSON layout of the text, made when first shown.
    pretty: Option<TextDoc>,
    /// Worth offering the JSON layout.
    pub json: bool,
    /// Why the text is not JSON, once all of it is loaded.
    pub json_error: Option<String>,
    /// The rows of the wrapped text, for these columns and layout.
    rows: Option<(usize, bool, Vec<Row>)>,
}

impl TextBody {
    fn new(encoding: Encoding) -> Self {
        Self {
            decoder: Decoder::new(encoding),
            doc: TextDoc::default(),
            pretty: None,
            json: false,
            json_error: None,
            rows: None,
        }
    }

    pub fn encoding(&self) -> Encoding {
        self.decoder.encoding()
    }

    fn push(&mut self, bytes: &[u8]) {
        let mut text = String::with_capacity(bytes.len());
        self.decoder.push(bytes, &mut text);
        self.doc.push(&text);
        self.pretty = None;
        self.rows = None;
    }

    /// The text as shown: laid out as JSON or as it is.
    pub fn shown(&mut self, pretty: bool) -> &TextDoc {
        if !pretty {
            return &self.doc;
        }
        let doc = &self.doc;
        self.pretty.get_or_insert_with(|| TextDoc::from_text(&decode::pretty_json(doc.text())))
    }

    /// The shown text and its rows, as last cut by [`TextBody::rows`].
    pub fn laid_out(&self, pretty: bool) -> (&TextDoc, &[Row]) {
        let doc = if pretty { self.pretty.as_ref().unwrap_or(&self.doc) } else { &self.doc };
        (doc, self.rows.as_ref().map_or(&[], |(_, _, rows)| rows))
    }

    /// The rows of the shown text cut at `columns` characters.
    pub fn rows(&mut self, pretty: bool, columns: usize) -> &[Row] {
        let fresh = self.rows.as_ref().is_some_and(|(cached, laid, _)| *cached == columns && *laid == pretty);
        if !fresh {
            let rows = decode::wrap(self.shown(pretty), columns);
            self.rows = Some((columns, pretty, rows));
        }
        self.rows.as_ref().map_or(&[], |(_, _, rows)| rows)
    }
}

pub struct ImageBody {
    pub texture: TextureHandle,
    /// The size of the image itself, before any scaling for display.
    pub width: u32,
    pub height: u32,
    pub format: &'static str,
}

pub enum Body {
    Loading,
    Text(TextBody),
    /// Not text: shown as bytes.
    Hex,
    Image(ImageBody),
    Failed(String),
}

enum Fetched {
    Bytes(Chunk),
    Image {
        image: ColorImage,
        width: u32,
        height: u32,
        format: &'static str,
        size: u64,
    },
    /// An image that could not be decoded: shown as bytes, with why.
    BadImage { chunk: Chunk, error: String },
}

pub struct Preview {
    pub origin: Origin,
    pub path: RemotePath,
    /// The file as listed.
    pub entry: Entry,
    storage: Arc<dyn Storage>,
    spawner: Spawner,
    pub body: Body,
    /// What was read so far, from the start of the file.
    pub bytes: Vec<u8>,
    /// The size of the file as the server last said.
    pub size: Option<u64>,
    /// Show the bytes of a text.
    pub hex: bool,
    /// Lay JSON out.
    pub pretty: bool,
    pub wrap: bool,
    /// Images at their own size rather than fitted.
    pub actual_size: bool,
    /// Said above the content: why it shows as it does.
    pub notice: Option<String>,
    task: Option<Task<Result<Fetched, String>>>,
}

impl Preview {
    pub fn open(spawner: &Spawner, storage: Arc<dyn Storage>, origin: Origin, path: RemotePath, entry: Entry) -> Self {
        let image = is_image(&entry.name);
        let mut preview = Self {
            origin,
            path,
            entry,
            storage,
            spawner: spawner.clone(),
            body: Body::Loading,
            bytes: Vec::new(),
            size: None,
            hex: false,
            pretty: false,
            wrap: false,
            actual_size: false,
            notice: None,
            task: None,
        };
        match preview.entry.size {
            Some(size) if image && size > MAX_IMAGE => {
                preview.notice = Some(tr!(
                    "Image trop lourde pour l'aperçu ({size}) : voici ses premiers octets.",
                    size = format::size(size)
                ));
                preview.fetch(0, CHUNK, false);
            }
            _ if image => preview.fetch(0, MAX_IMAGE, true),
            _ => preview.fetch(0, CHUNK, false),
        }
        preview
    }

    pub fn name(&self) -> &str {
        &self.entry.name
    }

    fn fetch(&mut self, offset: u64, len: u64, image: bool) {
        let (storage, path) = (self.storage.clone(), self.path.clone());
        self.task = Some(self.spawner.task(async move {
            if offset == 0 {
                tokio::time::sleep(SETTLE).await;
            }
            let chunk = storage
                .read_range(&path, offset, len)
                .await
                .map_err(|error| format!("{error:#}"))?;
            if image {
                Ok(decode_image(chunk).await)
            } else {
                Ok(Fetched::Bytes(chunk))
            }
        }));
    }

    pub fn loading(&self) -> bool {
        self.task.is_some()
    }

    /// Everything of the file is here.
    pub fn complete(&self) -> bool {
        self.size.is_some_and(|size| self.bytes.len() as u64 >= size)
    }

    pub fn can_load_more(&self) -> bool {
        matches!(self.body, Body::Text(_) | Body::Hex)
            && self.task.is_none()
            && !self.complete()
            && (self.bytes.len() as u64) < MAX_LOADED
    }

    pub fn load_more(&mut self) {
        if self.can_load_more() {
            self.fetch(self.bytes.len() as u64, CHUNK, false);
        }
    }

    /// Takes in what the worker read, once it is there.
    pub fn pump(&mut self, ctx: &egui::Context) {
        let Some(outcome) = self.task.as_ref().and_then(Task::take) else {
            return;
        };
        self.task = None;
        match outcome {
            Err(error) if self.bytes.is_empty() => self.body = Body::Failed(error),
            Err(error) => self.notice = Some(tr!("Lecture de la suite impossible : {error}", error)),
            Ok(Fetched::Image {
                image,
                width,
                height,
                format,
                size,
            }) => {
                self.size = Some(size);
                let texture = ctx.load_texture(format!("preview {}", self.path), image, TextureOptions::LINEAR);
                self.body = Body::Image(ImageBody {
                    texture,
                    width,
                    height,
                    format,
                });
            }
            Ok(Fetched::BadImage { chunk, error }) => {
                self.notice = Some(tr!("Image illisible ({error}) : voici ses octets.", error));
                self.take_bytes(chunk);
            }
            Ok(Fetched::Bytes(chunk)) => self.take_bytes(chunk),
        }
    }

    fn take_bytes(&mut self, chunk: Chunk) {
        self.size = Some(chunk.size);
        let first = self.bytes.is_empty();
        self.bytes.extend_from_slice(&chunk.bytes);
        if first {
            // Named otherwise, an image is still an image.
            if self.notice.is_none() && image_format(&self.bytes).is_some() && chunk.size <= MAX_IMAGE {
                self.bytes.clear();
                self.fetch(0, MAX_IMAGE, true);
                return;
            }
            self.body = match decode::sniff(&self.bytes) {
                Some(encoding) => {
                    let mut body = TextBody::new(encoding);
                    body.push(&chunk.bytes);
                    body.json = decode::looks_like_json(&self.entry.name, body.doc.text());
                    self.wrap = body.doc.widest() > WRAP_WIDER_THAN;
                    Body::Text(body)
                }
                None => Body::Hex,
            };
        } else if let Body::Text(body) = &mut self.body {
            body.push(&chunk.bytes);
        }
        let complete = self.complete();
        if let Body::Text(body) = &mut self.body
            && body.json
            && complete
        {
            body.json_error = decode::json_error(body.doc.text());
        }
    }

    /// Shows bytes that did not look like text as text all the same.
    pub fn force_text(&mut self) {
        if matches!(self.body, Body::Hex) {
            let mut body = TextBody::new(Encoding::Utf8);
            body.push(&self.bytes);
            self.body = Body::Text(body);
        }
        self.hex = false;
    }
}

fn is_image(name: &str) -> bool {
    let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "jfif" | "gif" | "webp" | "bmp" | "ico")
}

/// The image formats the preview decodes, by their first bytes.
fn image_format(bytes: &[u8]) -> Option<&'static str> {
    use image::ImageFormat;
    match image::guess_format(bytes).ok()? {
        ImageFormat::Png => Some("PNG"),
        ImageFormat::Jpeg => Some("JPEG"),
        ImageFormat::Gif => Some("GIF"),
        ImageFormat::WebP => Some("WebP"),
        ImageFormat::Bmp => Some("BMP"),
        ImageFormat::Ico => Some("ICO"),
        _ => None,
    }
}

/// Decodes an image off the async threads, scaled down to fit a texture.
async fn decode_image(chunk: Chunk) -> Fetched {
    let decoded = tokio::task::spawn_blocking(move || {
        let result = decode_pixels(&chunk.bytes);
        (chunk, result)
    })
    .await;
    match decoded {
        Ok((chunk, Ok((image, width, height, format)))) => Fetched::Image {
            image,
            width,
            height,
            format,
            size: chunk.size,
        },
        Ok((chunk, Err(error))) => Fetched::BadImage { chunk, error },
        Err(error) => Fetched::BadImage {
            chunk: Chunk::default(),
            error: error.to_string(),
        },
    }
}

fn decode_pixels(bytes: &[u8]) -> Result<(ColorImage, u32, u32, &'static str), String> {
    let format = image_format(bytes).ok_or_else(|| tr!("format non reconnu").to_owned())?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    reader.limits(limits);
    let image = reader.decode().map_err(|error| error.to_string())?;
    let (width, height) = (image.width(), image.height());
    let image = if width.max(height) > TEXTURE_SIDE {
        image.resize(TEXTURE_SIDE, TEXTURE_SIDE, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok((ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), width, height, format))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_told_by_name_and_by_content() {
        assert!(is_image("photo.JPG") && is_image("a.webp") && !is_image("notes.txt") && !is_image("png"));
        assert_eq!(image_format(b"\x89PNG\r\n\x1a\n...."), Some("PNG"));
        assert_eq!(image_format(b"GIF89a...."), Some("GIF"));
        assert_eq!(image_format(b"hello"), None);
    }

    #[test]
    fn a_small_image_decodes_and_a_broken_one_does_not() {
        let mut png = Vec::new();
        let pixels = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        pixels
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let (image, width, height, format) = decode_pixels(&png).unwrap();
        assert_eq!((width, height, format, image.size), (3, 2, "PNG", [3, 2]));
        assert!(decode_pixels(&png[..20]).is_err());
        assert!(decode_pixels(b"not an image").is_err());
    }
}
