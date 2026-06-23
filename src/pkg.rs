//! macOS `.pkg` installer support.
//!
//! A flat `.pkg` is a XAR ("eXtensible ARchive") container. Layout:
//!
//!   [ XAR header (fixed size) ]
//!   [ zlib-compressed XML TOC ]
//!   [ heap: each entry's data, often itself compressed per-entry ]
//!
//! The TOC describes a tree of `<file>` elements (which may be directories
//! containing nested `<file>` elements, hence the path-stack walk below).
//! Each leaf `<file>` has a `<data>` block giving the entry's offset/length
//! *within the heap* (i.e. relative to `header_size + toc_length_compressed`)
//! plus its own compression encoding (almost always zlib for XAR itself).
//!
//! Inside a component's `<name>.pkg/` directory, the `Payload` entry is a
//! gzip-compressed `cpio` (`newc` format) archive containing the actual files
//! to be installed — this is the only entry we extract. `Scripts` (pre/post
//! install shell scripts) and `Bom` (bill of materials) are skipped; they
//! carry no game data and `Scripts` sometimes uses the `odc` cpio variant
//! instead of `newc`, which we deliberately don't support here.

use anyhow::{bail, Context, Result};
use flate2::read::{GzDecoder, ZlibDecoder};
use indicatif::{ProgressBar, ProgressStyle};
use quick_xml::events::Event as XmlEvent;
use quick_xml::reader::Reader as XmlReader;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::mojo::{
    finalize_output, prepare_output_layout, ArcMmap, Cancelled, OutputLayout, StagingCleanup,
};

// ---------------------------------------------------------------------------
// XAR fixed header
// ---------------------------------------------------------------------------

const XAR_MAGIC: &[u8] = b"xar!";

struct XarHeader {
    /// Size of this fixed header in bytes (offset where the compressed TOC
    /// begins). Apple's xar writes 28, but we trust the on-disk value.
    header_size: u16,
    toc_length_compressed: u64,
}

fn parse_xar_header(data: &[u8]) -> Result<XarHeader> {
    if data.len() < 28 || !data.starts_with(XAR_MAGIC) {
        bail!("Not a XAR archive (missing 'xar!' magic)");
    }

    let header_size = u16::from_be_bytes([data[4], data[5]]);
    // data[6..8]  = version (u16, unused here)
    let toc_length_compressed = u64::from_be_bytes(data[8..16].try_into().unwrap());
    // data[16..24] = TOC length uncompressed (unused — quick-xml streams it)
    // data[24..28] = checksum algorithm id (unused — we don't verify checksums)

    if (header_size as usize) < 28 {
        bail!("XAR header_size field ({header_size}) is smaller than the minimum 28 bytes");
    }
    if data.len() < header_size as usize + toc_length_compressed as usize {
        bail!("XAR file is truncated: TOC extends past end of file");
    }

    Ok(XarHeader {
        header_size,
        toc_length_compressed,
    })
}

/// Inflates and returns the XAR TOC as a UTF-8 XML string.
fn read_toc_xml(data: &[u8], header: &XarHeader) -> Result<String> {
    let toc_start = header.header_size as usize;
    let toc_end = toc_start + header.toc_length_compressed as usize;
    let compressed = &data[toc_start..toc_end];

    let mut xml = String::new();
    ZlibDecoder::new(compressed)
        .read_to_string(&mut xml)
        .context("Failed to inflate XAR table of contents")?;
    Ok(xml)
}

// ---------------------------------------------------------------------------
// TOC entry model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeapEncoding {
    None,
    Gzip,
    Zlib,
    Bzip2,
    Other,
}

impl HeapEncoding {
    fn from_style_attr(style: &str) -> Self {
        // xar encodes this as e.g. "application/x-gzip", "application/octet-stream".
        if style.contains("gzip") {
            HeapEncoding::Gzip
        } else if style.contains("zlib") || style.contains("x-zlib") {
            HeapEncoding::Zlib
        } else if style.contains("bzip2") {
            HeapEncoding::Bzip2
        } else if style.contains("octet-stream") {
            HeapEncoding::None
        } else {
            HeapEncoding::Other
        }
    }
}

/// Which kind of cpio-bearing leaf this entry is. Most `.pkg`s only have a
/// `Payload`; some script-only or oddly-packaged ones (often built with
/// non-Apple tooling like bomutils, recognizable by a `Scripts~` wrapper
/// directory inside the cpio) put the real files inside `Scripts` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Payload,
    Scripts,
}

impl std::fmt::Display for EntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntryKind::Payload => write!(f, "Payload"),
            EntryKind::Scripts => write!(f, "Scripts"),
        }
    }
}

/// A single leaf `<file>` entry of interest from the TOC: a `Payload` or
/// `Scripts` cpio blob inside some `<name>.pkg/` component directory, with
/// its full path (e.g. "Game.pkg/Payload") and its location/encoding within
/// the heap.
struct PayloadEntry {
    kind: EntryKind,
    /// Full slash-joined path from the TOC root, e.g. "GameName.pkg/Payload".
    full_path: String,
    heap_offset: u64,
    heap_length: u64,
    encoding: HeapEncoding,
}

/// Walks the XAR TOC XML and collects every `<file>` leaf named exactly
/// "Payload" or "Scripts", recording its full path and heap location.
///
/// Two subtleties make this trickier than a flat "grab text by tag name" walk:
///
/// 1. xar nests `<file>` elements directly inside their parent `<file>` to
///    model directories, and a `<file>`'s `<name>` child arrives *after* its
///    `Start` event — so we can't push onto a path stack at `Start(file)`
///    time (we don't know the name yet). Each stack frame starts as pending
///    and is resolved (path-stack pushed) the moment its own `<name>` text is
///    read; it's popped at the matching `End(file)`.
///
/// 2. A `<file>` element's offset/length live inside a nested `<data>` child,
///    *not* as direct children of `<file>` — and `<file>` may *also* contain
///    one or more `<ea>` (extended attribute, e.g. `com.apple.ResourceFork`)
///    blocks that have their own nested `<name>`/`<offset>`/`<length>`. An
///    earlier version of this walker captured offset/length/name "whichever
///    was seen last anywhere inside the `<file>`", which meant a `Payload`
///    entry carrying an `<ea>` block would have its real name and heap
///    location silently clobbered by the `<ea>`'s own offset/length/name,
///    causing it to be dropped. To avoid this we track the *element stack*
///    (not just the `<file>` stack) and only record offset/length into a
///    frame's `<data>` slot while we're directly inside that frame's own
///    `<data>` child — and only treat a `<name>` as the file's own name when
///    it's a direct child of `<file>` (i.e. the innermost open element is
///    itself the `<file>` we're tracking, not `<data>` or `<ea>`).
fn find_payload_entries(xml: &str) -> Result<Vec<PayloadEntry>> {
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries = Vec::new();

    /// One stack frame per currently-open `<file>` element.
    struct Frame {
        /// Set once this frame's own `<name>` text has been read and pushed
        /// onto `path_stack`; used so `End(file)` knows whether to pop, and
        /// to ensure only the *first* direct-child `<name>` is captured.
        pushed_name: bool,
        file_type: Option<String>,
        data_offset: Option<u64>,
        data_length: Option<u64>,
        encoding: HeapEncoding,
    }

    let mut path_stack: Vec<String> = Vec::new();
    let mut frame_stack: Vec<Frame> = Vec::new();

    // Generic element stack covering *every* open tag, not just <file> — used
    // to tell whether the element currently being read is a direct child of
    // the innermost open <file> (e.g. <name>, <type>, <data>) or nested
    // further inside something like <data> or <ea> that itself sits inside
    // the <file>.
    #[derive(PartialEq, Clone, Copy)]
    enum Tag {
        File,
        Data,
        Other,
    }
    let mut elem_stack: Vec<Tag> = Vec::new();

    #[derive(PartialEq, Clone, Copy)]
    enum TextTarget {
        None,
        Name,
        Type,
        Offset,
        Length,
    }
    let mut text_target = TextTarget::None;

    // True only when the innermost open element is a <data> that is itself a
    // direct child of the innermost open <file> — i.e. elem_stack ends in
    // [..., File, Data]. This scopes <offset>/<length>/<encoding> capture to
    // the file's own <data> block, excluding any sibling <ea> block's fields.
    fn in_own_data_block(elem_stack: &[Tag]) -> bool {
        matches!(elem_stack, [.., Tag::File, Tag::Data])
    }
    // True only when <name>/<type> read right now is a direct child of the
    // innermost open <file> (not nested inside <data> or <ea>).
    fn is_direct_file_child(elem_stack: &[Tag]) -> bool {
        matches!(elem_stack.last(), Some(Tag::File))
    }

    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader
            .read_event_into(&mut buf)
            .context("Malformed XAR TOC XML")?
        {
            XmlEvent::Eof => break,

            XmlEvent::Start(e) => {
                let local = e.local_name();
                let tag = match local.as_ref() {
                    b"file" => Tag::File,
                    b"data" => Tag::Data,
                    _ => Tag::Other,
                };

                match local.as_ref() {
                    b"file" => frame_stack.push(Frame {
                        pushed_name: false,
                        file_type: None,
                        data_offset: None,
                        data_length: None,
                        encoding: HeapEncoding::None,
                    }),
                    b"name" if is_direct_file_child(&elem_stack) => {
                        text_target = TextTarget::Name;
                    }
                    b"type" if is_direct_file_child(&elem_stack) => {
                        text_target = TextTarget::Type;
                    }
                    b"offset" if in_own_data_block(&elem_stack) => {
                        text_target = TextTarget::Offset;
                    }
                    b"length" if in_own_data_block(&elem_stack) => {
                        text_target = TextTarget::Length;
                    }
                    b"encoding" if in_own_data_block(&elem_stack) => {
                        if let Some(frame) = frame_stack.last_mut() {
                            for attr in e.attributes().flatten() {
                                if attr.key.local_name().as_ref() == b"style" {
                                    let val = attr.unescape_value().unwrap_or_default();
                                    frame.encoding = HeapEncoding::from_style_attr(&val);
                                }
                            }
                        }
                        text_target = TextTarget::None;
                    }
                    // Any other element — including <name>/<type>/<offset>/
                    // <length> that *don't* satisfy the guards above, e.g.
                    // ones nested inside an <ea> block — is explicitly not a
                    // text target, so its text content is ignored rather
                    // than polluting the current frame.
                    _ => text_target = TextTarget::None,
                }

                elem_stack.push(tag);
            }

            XmlEvent::Text(t) => {
                if text_target == TextTarget::None {
                    continue;
                }
                let text = t.unescape().unwrap_or_default().into_owned();
                let Some(frame) = frame_stack.last_mut() else {
                    continue;
                };
                match text_target {
                    TextTarget::Name => {
                        if !frame.pushed_name {
                            path_stack.push(text);
                            frame.pushed_name = true;
                        }
                    }
                    TextTarget::Type => frame.file_type = Some(text),
                    TextTarget::Offset => frame.data_offset = text.trim().parse().ok(),
                    TextTarget::Length => frame.data_length = text.trim().parse().ok(),
                    TextTarget::None => {}
                }
            }

            // Self-closing elements (no children, no text, e.g.
            // <encoding style="application/x-gzip"/>) arrive as `Empty`, not
            // `Start` — quick-xml does NOT also synthesize a `Start`+`End`
            // pair for these, so `encoding`'s `style` attribute must be read
            // here specifically or it is silently lost (this previously
            // caused every Payload entry's `encoding` to come back as
            // `HeapEncoding::None`, since `<encoding>` is always self-closed
            // in real xar archives).
            XmlEvent::Empty(e) => {
                let local = e.local_name();
                if local.as_ref() == b"encoding" && in_own_data_block(&elem_stack) {
                    if let Some(frame) = frame_stack.last_mut() {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"style" {
                                let val = attr.unescape_value().unwrap_or_default();
                                frame.encoding = HeapEncoding::from_style_attr(&val);
                            }
                        }
                    }
                }
            }

            XmlEvent::End(e) => {
                elem_stack.pop();
                text_target = TextTarget::None;

                let local = e.local_name();
                if local.as_ref() == b"file" {
                    let Some(frame) = frame_stack.pop() else {
                        continue;
                    };

                    let is_leaf_entry_of_interest = frame.file_type.as_deref() != Some("directory")
                        && matches!(
                            path_stack.last().map(String::as_str),
                            Some("Payload") | Some("Scripts")
                        );

                    if is_leaf_entry_of_interest {
                        if let (Some(offset), Some(length)) = (frame.data_offset, frame.data_length)
                        {
                            let kind = match path_stack.last().map(String::as_str) {
                                Some("Payload") => EntryKind::Payload,
                                Some("Scripts") => EntryKind::Scripts,
                                _ => unreachable!(
                                    "is_leaf_entry_of_interest guarantees Payload or Scripts"
                                ),
                            };
                            entries.push(PayloadEntry {
                                kind,
                                full_path: path_stack.join("/"),
                                heap_offset: offset,
                                heap_length: length,
                                encoding: frame.encoding,
                            });
                        }
                    }

                    // Pop this frame's name back off the path stack now that
                    // we've finished processing it and all of its children
                    // (if any) — restoring the parent's context.
                    if frame.pushed_name {
                        path_stack.pop();
                    }
                }
            }

            _ => {}
        }
    }

    Ok(entries)
}

// ---------------------------------------------------------------------------
// cpio (newc) reader
// ---------------------------------------------------------------------------

/// One decoded `newc` cpio header plus the entry's file name.
struct CpioEntry {
    name: String,
    mode: u32,
    file_size: u64,
}

const CPIO_NEWC_MAGIC: &[u8] = b"070701";
const CPIO_ODC_MAGIC: &[u8] = b"070707";
const CPIO_TRAILER: &str = "TRAILER!!!";

/// Reads one cpio header + name + body from `r`, auto-detecting whether the
/// stream is `newc` (magic "070701") or `odc` / "old character" (magic
/// "070707") format from the first 6 bytes, and dispatching accordingly.
/// Returns `Ok(None)` once the TRAILER!!! entry is hit (end of archive).
///
/// macOS `Payload` archives are always `newc`; `Scripts` archives are
/// sometimes `odc` instead — particularly ones produced by non-Apple
/// packaging tools (bomutils, etc.) rather than Apple's own `pkgbuild`.
fn read_cpio_entry<R: Read>(r: &mut R) -> Result<Option<(CpioEntry, Vec<u8>)>> {
    let mut magic = [0u8; 6];
    if read_exact_or_eof(r, &mut magic)? {
        return Ok(None);
    }

    if magic == *CPIO_NEWC_MAGIC {
        read_cpio_entry_newc(r)
    } else if magic == *CPIO_ODC_MAGIC {
        read_cpio_entry_odc(r)
    } else {
        bail!(
            "Unsupported cpio variant (expected newc magic '070701' or odc \
             magic '070707', got {:?})",
            String::from_utf8_lossy(&magic)
        );
    }
}

/// `newc` layout per entry (magic already consumed by the caller):
///   - 13 fields of 8 ASCII-hex-encoded chars each (ino, mode, uid, gid,
///     nlink, mtime, filesize, devmajor, devminor, rdevmajor, rdevminor,
///     namesize, check)
///   - name (namesize bytes, NUL-terminated), padded to a 4-byte boundary
///     from the start of the header
///   - file data (filesize bytes), padded to a 4-byte boundary
fn read_cpio_entry_newc<R: Read>(r: &mut R) -> Result<Option<(CpioEntry, Vec<u8>)>> {
    // 13 hex fields, 8 chars each.
    let mut fields = [0u32; 13];
    for field in &mut fields {
        let mut buf = [0u8; 8];
        r.read_exact(&mut buf).context("Truncated cpio header")?;
        let s = std::str::from_utf8(&buf).context("Non-ASCII cpio header field")?;
        *field = u32::from_str_radix(s, 16).context("Malformed cpio header field")?;
    }

    let mode = fields[1];
    let file_size = fields[6] as u64;
    let name_size = fields[11] as usize;

    let mut name_buf = vec![0u8; name_size];
    r.read_exact(&mut name_buf).context("Truncated cpio name")?;
    // name_buf includes a trailing NUL; strip it.
    let name = String::from_utf8_lossy(&name_buf)
        .trim_end_matches('\0')
        .to_owned();

    // Header (6 magic + 104 fields) + name is padded to a 4-byte boundary.
    let header_and_name = 6 + 13 * 8 + name_size;
    skip_padding_newc(r, header_and_name)?;

    if name == CPIO_TRAILER {
        return Ok(None);
    }

    let mut body = vec![0u8; file_size as usize];
    r.read_exact(&mut body).context("Truncated cpio body")?;
    skip_padding_newc(r, file_size as usize)?;

    Ok(Some((
        CpioEntry {
            name,
            mode,
            file_size,
        },
        body,
    )))
}

/// `odc` ("old character" / portable ASCII) layout per entry (magic already
/// consumed by the caller). Per SUSv2 / `cpio(5)`:
///
///   struct cpio_odc_header {
///       char c_dev[6];      char c_ino[6];   char c_mode[6];
///       char c_uid[6];      char c_gid[6];   char c_nlink[6];
///       char c_rdev[6];     char c_mtime[11];
///       char c_namesize[6]; char c_filesize[11];
///   };
///
/// All fields are zero-padded ASCII *octal* (not hex) — nine 6-char fields
/// plus two 11-char fields (mtime, filesize) — followed by the NUL-terminated
/// name and then the raw file body, with **no padding/alignment anywhere**:
/// unlike `newc`, odc does not round the header+name or the body up to any
/// byte boundary.
fn read_cpio_entry_odc<R: Read>(r: &mut R) -> Result<Option<(CpioEntry, Vec<u8>)>> {
    fn read_octal_field<R: Read>(r: &mut R, width: usize, label: &str) -> Result<u64> {
        let mut buf = [0u8; 11]; // max field width used by odc (mtime/filesize)
        let slice = &mut buf[..width];
        r.read_exact(slice)
            .with_context(|| format!("Truncated cpio header field '{label}'"))?;
        let s = std::str::from_utf8(slice)
            .with_context(|| format!("Non-ASCII cpio header field '{label}'"))?;
        // Fields are octal, zero-padded on the left; tolerate a leading run
        // of spaces too, since some writers pad with spaces instead of '0'.
        u64::from_str_radix(s.trim(), 8)
            .with_context(|| format!("Malformed cpio header field '{label}': {s:?}"))
    }

    let _dev = read_octal_field(r, 6, "dev")?;
    let _ino = read_octal_field(r, 6, "ino")?;
    let mode = read_octal_field(r, 6, "mode")? as u32;
    let _uid = read_octal_field(r, 6, "uid")?;
    let _gid = read_octal_field(r, 6, "gid")?;
    let _nlink = read_octal_field(r, 6, "nlink")?;
    let _rdev = read_octal_field(r, 6, "rdev")?;
    let _mtime = read_octal_field(r, 11, "mtime")?;
    let name_size = read_octal_field(r, 6, "namesize")? as usize;
    let file_size = read_octal_field(r, 11, "filesize")?;

    let mut name_buf = vec![0u8; name_size];
    r.read_exact(&mut name_buf).context("Truncated cpio name")?;
    // name_size includes the trailing NUL terminator; strip it.
    let name = String::from_utf8_lossy(&name_buf)
        .trim_end_matches('\0')
        .to_owned();

    // No padding after the name in odc — body starts immediately.
    if name == CPIO_TRAILER {
        return Ok(None);
    }

    let mut body = vec![0u8; file_size as usize];
    r.read_exact(&mut body).context("Truncated cpio body")?;
    // No padding after the body either.

    Ok(Some((
        CpioEntry {
            name,
            mode,
            file_size,
        },
        body,
    )))
}

fn skip_padding_newc<R: Read>(r: &mut R, len_so_far: usize) -> Result<()> {
    let remainder = len_so_far % 4;
    if remainder != 0 {
        let pad = 4 - remainder;
        let mut buf = [0u8; 3];
        r.read_exact(&mut buf[..pad])
            .context("Truncated cpio padding")?;
    }
    Ok(())
}

/// Reads exactly `buf.len()` bytes, returning `Ok(true)` if the stream was
/// already at EOF (zero bytes read) rather than erroring — used to detect the
/// natural end of the cpio stream after the TRAILER entry's padding.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool> {
    let mut total = 0;
    while total < buf.len() {
        let n = r.read(&mut buf[total..])?;
        if n == 0 {
            if total == 0 {
                return Ok(true);
            }
            bail!("Truncated cpio stream (EOF mid-header)");
        }
        total += n;
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Listing / counting
// ---------------------------------------------------------------------------

/// Decompresses a Payload entry's heap bytes into a plain byte buffer,
/// applying whatever encoding the TOC declared (almost always gzip).
fn decode_payload_bytes(
    mmap: &ArcMmap,
    header: &XarHeader,
    entry: &PayloadEntry,
) -> Result<Vec<u8>> {
    let heap_start = header.header_size as usize + header.toc_length_compressed as usize;
    let start = heap_start + entry.heap_offset as usize;
    let end = start + entry.heap_length as usize;
    let data = mmap.as_ref();
    if end > data.len() {
        bail!(
            "Payload entry '{}' extends past end of file (heap range {start}..{end}, file len {})",
            entry.full_path,
            data.len()
        );
    }
    let raw = &data[start..end];

    // First, undo whatever compression XAR itself applied to the heap blob
    // (per the TOC's <data><encoding style="..."/>). This is a SEPARATE
    // compression layer from the cpio archive's own compression below — XAR
    // can store the blob compressed *or* raw (octet-stream) independently of
    // what that blob's bytes represent once XAR-decoded.
    let mut xar_decoded = Vec::new();
    match entry.encoding {
        HeapEncoding::Gzip => {
            GzDecoder::new(raw)
                .read_to_end(&mut xar_decoded)
                .with_context(|| format!("Failed to gunzip XAR heap blob '{}'", entry.full_path))?;
        }
        HeapEncoding::Zlib => {
            ZlibDecoder::new(raw)
                .read_to_end(&mut xar_decoded)
                .with_context(|| {
                    format!("Failed to inflate XAR heap blob '{}'", entry.full_path)
                })?;
        }
        HeapEncoding::None => {
            xar_decoded = raw.to_vec();
        }
        HeapEncoding::Bzip2 | HeapEncoding::Other => {
            bail!(
                "Payload entry '{}' uses an unsupported XAR heap encoding",
                entry.full_path
            );
        }
    }

    // The cpio archive itself is conventionally gzip-compressed in real
    // .pkg files — but that's a property of the cpio file's own bytes, not
    // something XAR's <encoding> attribute describes (it describes the heap
    // blob, which may be stored as octet-stream / uncompressed even though
    // the blob's *contents* are a gzip file). So rather than trusting
    // entry.encoding here, detect gzip magic (1F 8B) directly on the
    // XAR-decoded bytes and gunzip if present; otherwise assume the bytes
    // are already a raw cpio stream.
    const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
    let out = if xar_decoded.starts_with(&GZIP_MAGIC) {
        let mut inner = Vec::new();
        GzDecoder::new(xar_decoded.as_slice())
            .read_to_end(&mut inner)
            .with_context(|| format!("Failed to gunzip cpio stream '{}'", entry.full_path))?;
        inner
    } else {
        xar_decoded
    };
    Ok(out)
}

/// Walks a decoded Payload's cpio stream, calling `on_entry` for each file
/// (directories and the trailer are not passed to the callback).
fn for_each_cpio_file(
    payload_bytes: &[u8],
    mut on_entry: impl FnMut(&CpioEntry, &[u8]) -> Result<()>,
) -> Result<u64> {
    let mut cursor = std::io::Cursor::new(payload_bytes);
    let mut count = 0u64;
    while let Some((entry, body)) = read_cpio_entry(&mut cursor)? {
        // cpio mode high bits encode the file type (S_IFMT); 0o170000 mask,
        // 0o040000 = directory. We only care about regular-ish entries here;
        // directories are created implicitly from file paths during extract.
        const S_IFDIR: u32 = 0o040000;
        if entry.mode & 0o170000 == S_IFDIR {
            continue;
        }
        on_entry(&entry, &body)?;
        count += 1;
    }
    Ok(count)
}

pub fn list_lines(mmap: &ArcMmap) -> Result<Vec<String>> {
    let header = parse_xar_header(mmap.as_ref())?;
    let xml = read_toc_xml(mmap.as_ref(), &header)?;
    let payloads = select_extractable_entries(find_payload_entries(&xml)?);

    let mut lines = Vec::new();
    if payloads.is_empty() {
        bail!("No Payload or Scripts entries found in this .pkg (nothing to extract)");
    }

    for payload in &payloads {
        let bytes = decode_payload_bytes(mmap, &header, payload)?;
        lines.push(format!("== {} ({}) ==", payload.full_path, payload.kind));
        for_each_cpio_file(&bytes, |entry, _body| {
            lines.push(format!("{:>10}  {}", entry.file_size, entry.name));
            Ok(())
        })?;
    }
    Ok(lines)
}

pub fn list(mmap: &ArcMmap) -> Result<()> {
    for line in list_lines(mmap)? {
        println!("{line}");
    }
    Ok(())
}

/// Picks which entries to actually extract from the full set `find_payload_entries`
/// found. Per component directory (the part of `full_path` before the final
/// "/Payload" or "/Scripts"), `Payload` is preferred when present; `Scripts`
/// is only used as a fallback for components that have *no* `Payload` at all
/// — covering script-only `.pkg`s (sometimes built with non-Apple tooling,
/// recognizable by a `Scripts~` wrapper directory inside the cpio) whose real
/// files were packed into `Scripts` instead of a proper `Payload`. This keeps
/// normal `.pkg`s — which have both, with `Scripts` holding only pre/post
/// install shell scripts — from having that script noise extracted alongside
/// the real `Payload` files.
fn select_extractable_entries(entries: Vec<PayloadEntry>) -> Vec<PayloadEntry> {
    use std::collections::HashMap;

    fn component_dir(full_path: &str) -> &str {
        full_path
            .rsplit_once('/')
            .map(|(dir, _leaf)| dir)
            .unwrap_or(full_path)
    }

    let mut by_component: HashMap<&str, Vec<&PayloadEntry>> = HashMap::new();
    for entry in &entries {
        by_component
            .entry(component_dir(&entry.full_path))
            .or_default()
            .push(entry);
    }

    let mut keep_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for candidates in by_component.values() {
        let chosen = candidates
            .iter()
            .find(|e| e.kind == EntryKind::Payload)
            .or_else(|| candidates.iter().find(|e| e.kind == EntryKind::Scripts));
        if let Some(chosen) = chosen {
            keep_paths.insert(chosen.full_path.clone());
        }
    }

    entries
        .into_iter()
        .filter(|e| keep_paths.contains(&e.full_path))
        .collect()
}

/// Counts total files across all Payload entries, for progress-bar totals.
/// Re-decompresses each Payload once just to count — acceptable since `.pkg`
/// payloads for games are extracted once per run anyway and this avoids
/// holding every decoded Payload in memory simultaneously.
fn count_total_files(mmap: &ArcMmap, header: &XarHeader, payloads: &[PayloadEntry]) -> Result<u64> {
    let mut total = 0u64;
    for payload in payloads {
        let bytes = decode_payload_bytes(mmap, header, payload)?;
        total += for_each_cpio_file(&bytes, |_entry, _body| Ok(()))?;
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Progress sink abstraction (mirrors mojo.rs's ProgressSink)
// ---------------------------------------------------------------------------

trait PkgSink: Sync {
    fn warn(&self, message: String);
    fn file_written(&self, files_done: u64, files_total: u64, name: &str);
}

/// Drives the full extraction: decode every Payload, walk its cpio stream,
/// and write each file into the staging directory. Single-threaded — cpio is
/// Resolves which entries will actually be extracted and counts their total
/// file count upfront, so callers can size a progress bar correctly *before*
/// starting the (single-threaded, sequential) extraction loop — unlike
/// mojo.rs's ZIP path, cpio gives no central directory to size a bar against
/// without this separate pre-scan.
fn plan_extraction(mmap: &ArcMmap) -> Result<(XarHeader, Vec<PayloadEntry>, u64)> {
    let header = parse_xar_header(mmap.as_ref())?;
    let xml = read_toc_xml(mmap.as_ref(), &header)?;
    let payloads = select_extractable_entries(find_payload_entries(&xml)?);

    if payloads.is_empty() {
        bail!("No Payload or Scripts entries found in this .pkg (nothing to extract)");
    }

    let files_total = count_total_files(mmap, &header, &payloads)?;
    Ok((header, payloads, files_total))
}

/// Drives the full extraction: decode every Payload, walk its cpio stream,
/// and write each file into the staging directory. Single-threaded — cpio is
/// an inherently sequential stream format (no index to parallelize against,
/// unlike ZIP's central directory), so unlike mojo.rs's rayon-based ZIP path
/// this runs on the calling thread, polling `running` between files.
fn extract_pkg_inner(
    mmap: &ArcMmap,
    header: &XarHeader,
    payloads: &[PayloadEntry],
    files_total: u64,
    layout: &OutputLayout,
    running: &Arc<AtomicBool>,
    sink: &dyn PkgSink,
) -> Result<bool> {
    let mut files_done = 0u64;

    // Most .pkg installers (including all GOG ones we've seen) carry exactly
    // one Payload, in which case files extract flat into the staging dir to
    // match mojo.rs's flattened MojoSetup layout. If a product archive bundles
    // multiple components (multiple Payload entries), flattening them all into
    // the same directory risks silently overwriting same-named files across
    // components — so in that case each component gets its own subdirectory,
    // named after its ".pkg" component directory (e.g. "Game.pkg" from the
    // full_path "Game.pkg/Payload").
    let namespace_by_component = payloads.len() > 1;

    for payload in payloads {
        if payload.kind == EntryKind::Scripts {
            sink.warn(format!(
                "'{}' has no Payload — extracting files from Scripts instead \
                 (non-standard .pkg; verify output looks correct)",
                payload
                    .full_path
                    .rsplit_once('/')
                    .map(|(dir, _)| dir)
                    .unwrap_or(&payload.full_path)
            ));
        }

        let bytes = decode_payload_bytes(mmap, header, payload)?;

        let component_dir = namespace_by_component.then(|| {
            payload
                .full_path
                .rsplit_once('/')
                .map(|(dir, _payload_leaf)| dir)
                .unwrap_or(payload.full_path.as_str())
                .to_owned()
        });

        let result = for_each_cpio_file(&bytes, |entry, body| {
            if !running.load(Ordering::Relaxed) {
                bail!(Cancelled);
            }

            // cpio paths are typically "./Applications/Game.app/...";
            // strip a leading "./" and reject absolute/parent-traversal
            // components the same way mojo.rs guards ZIP paths, since cpio
            // gives us no `enclosed_name()` helper to lean on.
            let Some(safe_rel) = sanitize_cpio_path(&entry.name) else {
                sink.warn(format!("Skipping unsafe path in cpio: {:?}", entry.name));
                return Ok(());
            };

            let outpath = match &component_dir {
                Some(dir) => layout.staging_dir.join(dir).join(&safe_rel),
                None => layout.staging_dir.join(&safe_rel),
            };
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create directory {}", parent.display()))?;
            }

            let outfile = File::create(&outpath)
                .with_context(|| format!("Failed to create {}", outpath.display()))?;
            let mut writer = BufWriter::new(outfile);
            writer
                .write_all(body)
                .with_context(|| format!("Failed to write {}", outpath.display()))?;
            writer
                .flush()
                .with_context(|| format!("Failed to flush {}", outpath.display()))?;

            #[cfg(unix)]
            {
                // Lower 12 bits of cpio mode are the standard permission bits.
                let perm_bits = entry.mode & 0o7777;
                if perm_bits != 0 {
                    fs::set_permissions(&outpath, fs::Permissions::from_mode(perm_bits))
                        .with_context(|| {
                            format!("Failed to set permissions on {}", outpath.display())
                        })?;
                }
            }

            files_done += 1;
            sink.file_written(files_done, files_total, &safe_rel.to_string_lossy());
            Ok(())
        });

        match result {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<Cancelled>().is_some() => return Ok(false),
            Err(e) => return Err(e),
        }
    }

    Ok(true)
}

/// Validates and normalizes a cpio entry path, rejecting absolute paths and
/// `..` traversal components — mirroring the protection `enclosed_name()`
/// gives the ZIP path in mojo.rs (RUSTSEC-2021-0080-style guard), since cpio
/// has no equivalent built-in helper.
fn sanitize_cpio_path(raw: &str) -> Option<PathBuf> {
    let stripped = raw.strip_prefix("./").unwrap_or(raw);
    if stripped.is_empty() {
        return None;
    }

    let mut safe = PathBuf::new();
    for component in Path::new(stripped).components() {
        use std::path::Component;
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if safe.as_os_str().is_empty() {
        None
    } else {
        Some(safe)
    }
}

// ---------------------------------------------------------------------------
// CLI extraction
// ---------------------------------------------------------------------------

struct CliSink {
    pb: ProgressBar,
}

impl PkgSink for CliSink {
    fn warn(&self, message: String) {
        self.pb.println(format!("Warning: {message}"));
    }

    fn file_written(&self, files_done: u64, _files_total: u64, name: &str) {
        self.pb.set_message(name.to_owned());
        self.pb.set_position(files_done);
    }
}

fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green.bold} Files [{bar:40.green/white.dim}] {pos}/{len}  {msg:.dim}",
    )
    .unwrap()
    .progress_chars("█▇▆▅▄▃▂ ")
}

pub fn extract(
    mmap: &ArcMmap,
    input_file: &Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>,
) -> Result<bool> {
    let _ = user_cancelled; // cancellation is observed via `running` inside the cpio loop

    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

    let (header, payloads, files_total) = plan_extraction(mmap)?;

    let layout = prepare_output_layout(&resolved_output_dir, force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    println!("Extracting to: {}\n", resolved_output_dir.display());
    println!("Press Ctrl+C to cancel.\n");

    let pb = ProgressBar::new(files_total);
    pb.set_style(progress_style());
    pb.enable_steady_tick(Duration::from_millis(100));

    let extraction_start = Instant::now();
    let sink = CliSink { pb: pb.clone() };

    let ok = extract_pkg_inner(
        mmap,
        &header,
        &payloads,
        files_total,
        &layout,
        running,
        &sink,
    )?;

    if !ok {
        pb.abandon_with_message("cancelled");
        drop(staging_cleanup);
        return Ok(false);
    }

    pb.finish_with_message("done");

    finalize_output(&layout)?;
    staging_cleanup.disarm();

    println!(
        "\n  {} files  in {:.1}s",
        pb.position(),
        extraction_start.elapsed().as_secs_f64()
    );

    Ok(true)
}

// ---------------------------------------------------------------------------
// GUI extraction path
// ---------------------------------------------------------------------------

#[cfg(feature = "gui")]
use crate::gui::GuiEvent;
#[cfg(feature = "gui")]
use std::sync::mpsc;

#[cfg(feature = "gui")]
struct GuiSink {
    tx: mpsc::Sender<GuiEvent>,
    files_total: u64,
    /// Shared with the caller so the real count survives past this sink's
    /// lifetime, for the final `GuiEvent::Done { file_count, .. }`.
    files_done_counter: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(feature = "gui")]
impl PkgSink for GuiSink {
    fn warn(&self, message: String) {
        let _ = self
            .tx
            .send(GuiEvent::Log(format!("Skipping unsafe path: {message}")));
    }

    fn file_written(&self, files_done: u64, files_total: u64, name: &str) {
        self.files_done_counter.store(files_done, Ordering::Relaxed);
        let _ = self.tx.send(GuiEvent::Progress {
            files_done,
            files_total: if files_total > 0 {
                files_total
            } else {
                self.files_total
            },
            current_file: Some(name.to_owned()),
        });
    }
}

#[cfg(feature = "gui")]
pub fn extract_gui(
    mmap: &crate::mojo::ArcMmap,
    input_file: &std::path::Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>, // <--- We need to monitor this!
    tx: &mpsc::Sender<GuiEvent>,
) -> Result<bool> {
    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

    let (header, payloads, files_total) = plan_extraction(mmap)?;

    let layout = prepare_output_layout(&resolved_output_dir, force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    let _ = tx.send(GuiEvent::Log(format!(
        "Extracting to: {}",
        resolved_output_dir.display()
    )));

    let extraction_start = Instant::now();
    let files_done_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sink = GuiSink {
        tx: tx.clone(),
        files_total,
        files_done_counter: files_done_counter.clone(),
    };

    // Pass the real `user_cancelled` down into the extractor instead of ignoring it
    let ok = extract_pkg_inner(
        mmap,
        &header,
        &payloads,
        files_total,
        &layout,
        running,
        &sink,
    )?;

    if !ok || user_cancelled.load(Ordering::Relaxed) {
        drop(staging_cleanup);
        // Explicitly return false so main.rs maps this to GuiEvent::Cancelled
        return Ok(false);
    }

    // Move the staged files into their final location and disarm cleanup —
    // mirrors the CLI path. Without this, the GUI reported success using the
    // *intended* output_dir while the files actually stayed in the staging
    // dir (or got deleted by StagingCleanup's Drop), so the folder the GUI
    // pointed at after extraction was wrong/empty.
    finalize_output(&layout)?;
    staging_cleanup.disarm();

    // Send a final progress tick to ensure the GUI reaches 100%.
    let _ = tx.send(GuiEvent::Progress {
        files_done: files_total,
        files_total,
        current_file: None,
    });

    let elapsed = extraction_start.elapsed().as_secs_f64();
    let _ = tx.send(GuiEvent::Done {
        elapsed_secs: elapsed,
        file_count: files_total as usize,
        output_dir: resolved_output_dir,
    });

    Ok(true)
}
