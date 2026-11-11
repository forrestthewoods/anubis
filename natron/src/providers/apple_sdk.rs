//! `apple_sdk` provider: the macOS SDK (headers, framework headers and `.tbd`
//! link stubs) from Apple's Command Line Tools packages, with no Mac, Xcode or
//! Apple ID. The approach is `xmac`'s (https://github.com/Jarred-Sumner/xmac):
//! the CLT ship one flat package per SDK on Apple's software-update CDN.
//!
//! ```toml
//! [[toolchain]]
//! name       = "macos_sdk"
//! deploy_dir = "macos_sdk"
//! provider   = "apple_sdk"
//! [toolchain.options]
//! sdk            = "MacOSX26.5.sdk"             # directory inside the package
//! package        = "CLTools_macOSNMOS_SDK.pkg"  # file name on Apple's CDN
//! payload_sha256 = "..."                        # pins the SDK's bytes
//! urls           = ["https://swcdn.apple.com/content/downloads/.../CLTools_macOSNMOS_SDK.pkg"]
//! ```
//!
//! **Why the pin is on the payload, not the package.** Apple's CDN URLs are
//! not durable: products drop out of the catalog and their URLs start
//! returning 403, and Apple re-publishes the same SDK under new product ids.
//! Re-published packages differ only in their signed table of contents; the
//! `Payload` member (the pbzx/cpio stream holding the SDK) is byte-identical.
//! So `payload_sha256` pins the content, and the provider is free to find it
//! anywhere:
//!
//!   1. each of `urls`, in order (put an org mirror first if you keep one);
//!   2. then every package named `package` in Apple's current catalog.
//!
//! The first package whose `Payload` hashes to `payload_sha256` wins. When
//! Apple drops an SDK entirely, the error says so and names the hash, so a
//! mirror copy (any URL, including `file://`) can stand in.
//!
//! `natron apple-sdk list` prints every SDK the catalog offers today, with the
//! values to paste here.
//!
//! The package format is a xar archive (28-byte header, zlib XML table of
//! contents, heap) whose `Payload` is a pbzx stream (xz chunks) of an odc
//! cpio archive. All three are small enough to read here; nothing from the
//! package is executed.
//!
//! **Pruning.** The SDK unpacks to ~840 MB. `exclude` (glob patterns over
//! SDK-relative paths; defaults in [`DEFAULT_EXCLUDES`]) drops what C, C++ and
//! Objective-C builds never read: Swift modules, Mac Catalyst frameworks, man
//! pages. Symlinks left dangling by pruning are dropped too.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use super::options::{optional_str, optional_string_list, required_str};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;

/// Provider id used in `[[toolchain]] provider = "apple_sdk"`.
pub const ID: &str = "apple_sdk";

/// Apple's merged software-update catalog; it lists every current CLT
/// release. Override with `catalog_url`.
pub const DEFAULT_CATALOG: &str = "https://swscan.apple.com/content/catalogs/others/index-26-15-14-13-12-10.16-10.15-10.14-10.13-10.12-10.11-10.10-10.9-mountainlion-lion-snowleopard-leopard.merged-1.sucatalog.gz";

/// SDK-relative paths no C/C++/Objective-C build reads. Matched with
/// `glob::Pattern`; a directory match drops everything under it.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "System/iOSSupport",
    "System/Cryptexes",
    "System/Library/Perl",
    "usr/share",
    "**/*.swiftmodule",
    "**/*.swiftinterface",
    "**/*.swiftdoc",
];

pub struct AppleSdkProvider {
    catalog_url: String,
}

impl AppleSdkProvider {
    pub fn new() -> Self {
        Self { catalog_url: DEFAULT_CATALOG.to_string() }
    }
}

impl Default for AppleSdkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub sdk: String,
    pub package: String,
    pub payload_sha256: String,
    pub urls: Vec<String>,
    pub catalog_url: Option<String>,
    pub use_catalog: bool,
    pub excludes: Vec<String>,
}

pub(crate) fn parse_options(options: &toml::Table) -> Result<Options> {
    let sdk = required_str(options, "sdk", ID)?.to_string();
    if sdk.contains('/') || sdk.contains('\\') || !sdk.ends_with(".sdk") {
        bail!("`apple_sdk` option 'sdk' must be a directory name like \"MacOSX26.5.sdk\", got '{sdk}'");
    }
    let package = required_str(options, "package", ID)?.to_string();
    let payload_sha256 = required_str(options, "payload_sha256", ID)?.to_ascii_lowercase();
    if payload_sha256.len() != 64 || !payload_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("`apple_sdk` option 'payload_sha256' must be 64 hex digits");
    }
    let urls = optional_string_list(options, "urls", ID)?;
    let catalog_url = optional_str(options, "catalog_url", ID)?.map(str::to_string);
    let use_catalog = match options.get("catalog") {
        None => true,
        Some(v) => v.as_bool().ok_or_else(|| anyhow!("`apple_sdk` option 'catalog' must be a bool"))?,
    };
    let excludes = if options.contains_key("exclude") {
        optional_string_list(options, "exclude", ID)?
    } else {
        DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect()
    };
    for e in &excludes {
        glob::Pattern::new(e).with_context(|| format!("`apple_sdk` exclude pattern '{e}'"))?;
    }
    if urls.is_empty() && !use_catalog {
        bail!("`apple_sdk`: no `urls` and `catalog = false` leaves nowhere to download from");
    }
    Ok(Options { sdk, package, payload_sha256, urls, catalog_url, use_catalog, excludes })
}

pub(crate) fn compute_fingerprint(o: &Options) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    // Where the bytes came from (urls, catalog) isn't content: leave it out,
    // so adding a mirror doesn't invalidate installs.
    let key = format!("{}\n{}\n{}", o.sdk, o.payload_sha256, o.excludes.join("\n"));
    let stem = o.sdk.trim_end_matches(".sdk");
    sanitize_fingerprint(&format!("apple-sdk-{stem}-{}-{:08x}", &o.payload_sha256[..12], xxh3_64(key.as_bytes()) as u32))
}

impl Provider for AppleSdkProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(&self, options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed> {
        let o = parse_options(options)?;
        let fingerprint = compute_fingerprint(&o);
        let display = format!("macOS SDK {} ({})", o.sdk, &o.payload_sha256[..12]);
        if ctx.cache().install_present(&fingerprint) {
            return Ok(Installed { fingerprint, display, options: options.clone(), freshly_extracted: false });
        }

        let (url, pkg) = find_package(ctx, &o, o.catalog_url.as_deref().unwrap_or(&self.catalog_url))?;
        let staging = ctx.staging_dir()?;
        let report = extract_sdk(&pkg, &o.sdk, &staging, &o.excludes)
            .with_context(|| format!("extracting {} from {url}", o.sdk))?;
        tracing::info!(
            "{}: {} files, {} symlinks ({} excluded, {} dangling symlinks dropped)",
            o.sdk, report.files, report.symlinks, report.excluded, report.dangling
        );

        let mut resolved = options.clone();
        resolved.insert("resolved_url".into(), url.into());
        Ok(Installed { fingerprint, display, options: resolved, freshly_extracted: true })
    }
}

/// Try `urls`, then the catalog, until a package's `Payload` matches the pin.
fn find_package(ctx: &InstallCtx, o: &Options, catalog_url: &str) -> Result<(String, PathBuf)> {
    let mut tried = Vec::new();
    let check = |url: &str, tried: &mut Vec<String>| -> Option<PathBuf> {
        match ctx.download(url, None).and_then(|p| payload_sha256(&p).map(|h| (p, h))) {
            Ok((p, h)) if h == o.payload_sha256 => Some(p),
            Ok((_, h)) => {
                tried.push(format!("{url}: payload sha256 {h}"));
                None
            }
            Err(e) => {
                tried.push(format!("{url}: {e:#}"));
                None
            }
        }
    };
    for url in &o.urls {
        if let Some(p) = check(url, &mut tried) {
            return Ok((url.clone(), p));
        }
    }
    if o.use_catalog {
        match catalog_packages(catalog_url) {
            Ok(entries) => {
                for e in entries.iter().filter(|e| e.url.ends_with(&format!("/{}", o.package))) {
                    if o.urls.contains(&e.url) {
                        continue;
                    }
                    if let Some(p) = check(&e.url, &mut tried) {
                        tracing::warn!(
                            "{}: found in Apple's catalog at {} (product {}); add it to `urls` to skip the catalog",
                            o.sdk, e.url, e.product
                        );
                        return Ok((e.url.clone(), p));
                    }
                }
            }
            Err(e) => tried.push(format!("catalog {catalog_url}: {e:#}")),
        }
    }
    bail!(
        "no package with payload sha256 {} ({}) found. Apple may have withdrawn this SDK; \
         `natron apple-sdk list` shows what it offers now, or put a saved copy of {} first in `urls`.\n  {}",
        o.payload_sha256,
        o.sdk,
        o.package,
        tried.join("\n  ")
    )
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One package listed in Apple's software-update catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPackage {
    pub product: String,
    pub post_date: String,
    pub url: String,
    pub size: Option<u64>,
}

/// Every CLT package in the catalog, newest product first.
pub fn catalog_packages(catalog_url: &str) -> Result<Vec<CatalogPackage>> {
    let mut resp = crate::download::agent()
        .get(catalog_url)
        .call()
        .with_context(|| format!("GET {catalog_url}"))?;
    let mut raw = Vec::new();
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut raw)
        .with_context(|| format!("reading {catalog_url}"))?;
    let text = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut s = String::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8(raw).context("catalog is not UTF-8")?
    };
    Ok(parse_catalog(&text))
}

/// Pull CLT packages out of the catalog plist without a plist parser: each
/// product is `<key>ID</key><dict>…</dict>` under `Products`, holding a
/// `Packages` array of dicts with `URL` and `Size`, and a `PostDate`.
pub(crate) fn parse_catalog(text: &str) -> Vec<CatalogPackage> {
    let mut out = Vec::new();
    let Some(products_at) = text.find("<key>Products</key>") else {
        return out;
    };
    let products = &text[products_at..];
    // Product ids look like `047-91568`; scan each `<key>ID</key>\n<dict>`.
    let mut rest = products;
    while let Some(k) = rest.find("<key>") {
        let after = &rest[k + 5..];
        let Some(end) = after.find("</key>") else { break };
        let id = &after[..end];
        let body_start = &after[end + 6..];
        let trimmed = body_start.trim_start();
        if !trimmed.starts_with("<dict>") || !looks_like_product_id(id) {
            rest = body_start;
            continue;
        }
        let dict_len = matching_dict_len(trimmed);
        let dict = &trimmed[..dict_len];
        if dict.contains("/CLTools_") {
            let post_date = value_after_key(dict, "PostDate", "date").unwrap_or_default();
            for pkg in dict.split("<dict>").skip(1) {
                if let Some(url) = value_after_key(pkg, "URL", "string") {
                    if url.contains("/CLTools_") {
                        let size = value_after_key(pkg, "Size", "integer").and_then(|s| s.parse().ok());
                        out.push(CatalogPackage { product: id.to_string(), post_date: post_date.clone(), url, size });
                    }
                }
            }
        }
        rest = &trimmed[dict_len..];
    }
    out.sort_by(|a, b| b.post_date.cmp(&a.post_date).then(a.url.cmp(&b.url)));
    out
}

fn looks_like_product_id(s: &str) -> bool {
    s.len() >= 7 && s.contains('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Length of the `<dict>…</dict>` that `s` starts with, nesting included.
fn matching_dict_len(s: &str) -> usize {
    let mut depth = 0usize;
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with("<dict>") {
            depth += 1;
            i += 6;
        } else if s[i..].starts_with("</dict>") {
            depth -= 1;
            i += 7;
            if depth == 0 {
                return i;
            }
        } else if s[i..].starts_with("<dict/>") {
            i += 7;
        } else {
            i += s[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
    }
    s.len()
}

/// `<key>KEY</key> <TAG>value</TAG>` → `value` (first occurrence).
fn value_after_key(s: &str, key: &str, tag: &str) -> Option<String> {
    let k = s.find(&format!("<key>{key}</key>"))?;
    let rest = s[k..].split_once("</key>")?.1.trim_start();
    let open = format!("<{tag}>");
    let rest = rest.strip_prefix(&open)?;
    let end = rest.find(&format!("</{tag}>"))?;
    Some(xml_unescape(&rest[..end]))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// xar
// ---------------------------------------------------------------------------

/// Location of one member in a xar archive's heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XarMember {
    pub offset: u64,
    pub length: u64,
    pub encoding: String,
}

/// Read a xar header + table of contents and locate the top-level member
/// `name`.
pub(crate) fn xar_member<R: Read + Seek>(r: &mut R, name: &str) -> Result<XarMember> {
    let mut hdr = [0u8; 28];
    r.read_exact(&mut hdr).context("reading xar header")?;
    if &hdr[0..4] != b"xar!" {
        bail!("not a xar archive (bad magic)");
    }
    let header_size = u16::from_be_bytes([hdr[4], hdr[5]]) as u64;
    let toc_compressed = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
    if toc_compressed > 64 << 20 {
        bail!("xar table of contents is implausibly large ({toc_compressed} bytes)");
    }
    r.seek(SeekFrom::Start(header_size))?;
    let mut toc_z = vec![0u8; toc_compressed as usize];
    r.read_exact(&mut toc_z).context("reading xar table of contents")?;
    let mut toc = String::new();
    flate2::read::ZlibDecoder::new(&toc_z[..])
        .read_to_string(&mut toc)
        .context("inflating xar table of contents")?;
    let heap = header_size + toc_compressed;
    let m = toc_member(&toc, name).ok_or_else(|| anyhow!("xar archive has no '{name}' member"))?;
    Ok(XarMember { offset: heap + m.offset, ..m })
}

/// Find `<file>` whose `<name>` is `name` in a xar TOC. Each `<file …>`
/// element is taken up to the next `<file` (members we want aren't nested).
pub(crate) fn toc_member(toc: &str, name: &str) -> Option<XarMember> {
    let want = format!("<name>{name}</name>");
    for chunk in toc.split("<file ").skip(1) {
        let chunk = chunk.split("<file ").next().unwrap_or(chunk);
        if !chunk.contains(&want) {
            continue;
        }
        let data = &chunk[chunk.find("<data>")?..];
        let tag = |t: &str| -> Option<u64> {
            let s = data.find(&format!("<{t}>"))? + t.len() + 2;
            let e = data[s..].find(&format!("</{t}>"))?;
            data[s..s + e].trim().parse().ok()
        };
        let encoding = data
            .find("<encoding style=\"")
            .map(|i| {
                let s = &data[i + 17..];
                s[..s.find('"').unwrap_or(0)].to_string()
            })
            .unwrap_or_default();
        return Some(XarMember { offset: tag("offset")?, length: tag("length")?, encoding });
    }
    None
}

/// SHA-256 of a package's `Payload` member bytes, as stored (still pbzx).
pub fn payload_sha256(pkg: &Path) -> Result<String> {
    let mut f = std::fs::File::open(pkg).with_context(|| format!("opening {}", pkg.display()))?;
    let m = xar_member(&mut f, "Payload").with_context(|| format!("reading {}", pkg.display()))?;
    f.seek(SeekFrom::Start(m.offset))?;
    let mut h = Sha256::new();
    std::io::copy(&mut (&mut f).take(m.length), &mut h)?;
    Ok(format!("{:x}", h.finalize()))
}

// ---------------------------------------------------------------------------
// pbzx
// ---------------------------------------------------------------------------

/// Streams the decompressed bytes of a pbzx stream: `pbzx`, an 8-byte flags
/// word, then chunks of (8-byte uncompressed size, 8-byte stored size, data),
/// each data block xz-compressed unless both sizes are equal.
pub(crate) struct PbzxReader<R: Read> {
    inner: R,
    remaining: u64,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: Read> PbzxReader<R> {
    /// `len` is the number of bytes of `inner` that belong to the stream.
    pub fn new(mut inner: R, len: u64) -> Result<Self> {
        let mut magic = [0u8; 12];
        inner.read_exact(&mut magic).context("reading pbzx header")?;
        if &magic[..4] != b"pbzx" {
            bail!("Payload is not a pbzx stream");
        }
        Ok(Self { inner, remaining: len.saturating_sub(12), buf: Vec::new(), pos: 0 })
    }

    fn next_chunk(&mut self) -> std::io::Result<bool> {
        if self.remaining < 16 {
            return Ok(false);
        }
        let mut h = [0u8; 16];
        self.inner.read_exact(&mut h)?;
        let unpacked = u64::from_be_bytes(h[..8].try_into().unwrap());
        let stored = u64::from_be_bytes(h[8..].try_into().unwrap());
        if stored > self.remaining - 16 || stored > 1 << 30 || unpacked > 1 << 30 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad pbzx chunk size"));
        }
        self.remaining -= 16 + stored;
        let mut data = vec![0u8; stored as usize];
        self.inner.read_exact(&mut data)?;
        self.buf = if stored == unpacked {
            data
        } else {
            let mut out = Vec::with_capacity(unpacked as usize);
            liblzma::read::XzDecoder::new(&data[..]).read_to_end(&mut out)?;
            out
        };
        self.pos = 0;
        Ok(true)
    }
}

impl<R: Read> Read for PbzxReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            if !self.next_chunk()? {
                return Ok(0);
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// cpio (odc) + extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ExtractReport {
    pub files: usize,
    pub symlinks: usize,
    pub excluded: usize,
    pub dangling: usize,
}

/// Extract `SDKs/<sdk>/` from a CLT package into `dest` (which becomes the
/// SDK root), skipping `excludes`.
pub(crate) fn extract_sdk(pkg: &Path, sdk: &str, dest: &Path, excludes: &[String]) -> Result<ExtractReport> {
    let mut f = std::fs::File::open(pkg).with_context(|| format!("opening {}", pkg.display()))?;
    let m = xar_member(&mut f, "Payload")?;
    if !m.encoding.is_empty() && m.encoding != "application/octet-stream" {
        bail!("unsupported Payload encoding '{}'", m.encoding);
    }
    f.seek(SeekFrom::Start(m.offset))?;
    let stream = PbzxReader::new(std::io::BufReader::new(f).take(m.length), m.length)?;
    extract_cpio_sdk(stream, sdk, dest, excludes)
}

/// Names of the SDK directories (`MacOSX26.5.sdk`, …) a CLT package holds.
/// Symlinked aliases such as `MacOSX26.sdk` are left out.
pub fn sdk_names(pkg: &Path) -> Result<Vec<String>> {
    let mut f = std::fs::File::open(pkg).with_context(|| format!("opening {}", pkg.display()))?;
    let m = xar_member(&mut f, "Payload")?;
    f.seek(SeekFrom::Start(m.offset))?;
    let mut r = PbzxReader::new(std::io::BufReader::new(f).take(m.length), m.length)?;
    let mut out = Vec::new();
    loop {
        let mut h = [0u8; 76];
        r.read_exact(&mut h).context("reading cpio header")?;
        if &h[..6] != b"070707" {
            bail!("not an odc cpio archive");
        }
        let oct = |range: std::ops::Range<usize>| -> u64 {
            std::str::from_utf8(&h[range]).ok().and_then(|s| u64::from_str_radix(s, 8).ok()).unwrap_or(0)
        };
        let (mode, namesize, filesize) = (oct(18..24), oct(59..65) as usize, oct(65..76));
        let mut name = vec![0u8; namesize];
        r.read_exact(&mut name)?;
        let name = String::from_utf8_lossy(&name[..namesize.saturating_sub(1)]).into_owned();
        if name == "TRAILER!!!" {
            break;
        }
        if mode & 0o170000 == 0o040000 {
            if let Some((_, last)) = name.rsplit_once("/SDKs/") {
                if last.ends_with(".sdk") && !last.contains('/') {
                    out.push(last.to_string());
                }
            }
        }
        std::io::copy(&mut (&mut r).take(filesize), &mut std::io::sink())?;
    }
    Ok(out)
}

pub(crate) fn extract_cpio_sdk<R: Read>(mut r: R, sdk: &str, dest: &Path, excludes: &[String]) -> Result<ExtractReport> {
    let patterns: Vec<glob::Pattern> = excludes.iter().map(|e| glob::Pattern::new(e)).collect::<Result<_, _>>()?;
    let marker = format!("SDKs/{sdk}");
    let mut report = ExtractReport::default();
    let mut symlinks: Vec<(PathBuf, String)> = Vec::new();
    std::fs::create_dir_all(dest)?;
    loop {
        let mut h = [0u8; 76];
        r.read_exact(&mut h).context("reading cpio header (truncated archive?)")?;
        if &h[..6] != b"070707" {
            bail!("not an odc cpio archive");
        }
        let oct = |range: std::ops::Range<usize>| -> Result<u64> {
            let s = std::str::from_utf8(&h[range]).map_err(|_| anyhow!("bad cpio header"))?;
            u64::from_str_radix(s, 8).map_err(|_| anyhow!("bad cpio header field '{s}'"))
        };
        let mode = oct(18..24)?;
        let namesize = oct(59..65)? as usize;
        let filesize = oct(65..76)?;
        if namesize == 0 || namesize > 4096 {
            bail!("bad cpio name size {namesize}");
        }
        let mut name = vec![0u8; namesize];
        r.read_exact(&mut name)?;
        let name = String::from_utf8_lossy(&name[..namesize - 1]).into_owned();
        if name == "TRAILER!!!" {
            break;
        }
        let rel = sdk_relative(&name, &marker);
        let kind = mode & 0o170000;
        let Some(rel) = rel else {
            std::io::copy(&mut (&mut r).take(filesize), &mut std::io::sink())?;
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !rel_str.is_empty() && is_excluded(&patterns, &rel_str) {
            report.excluded += 1;
            std::io::copy(&mut (&mut r).take(filesize), &mut std::io::sink())?;
            continue;
        }
        let out = dest.join(&rel);
        match kind {
            0o040000 => std::fs::create_dir_all(&out)?,
            0o120000 => {
                if filesize > 4096 {
                    bail!("cpio symlink target too long in {name}");
                }
                let mut t = vec![0u8; filesize as usize];
                r.read_exact(&mut t)?;
                let target = String::from_utf8(t).map_err(|_| anyhow!("non-UTF-8 symlink target in {name}"))?;
                if !link_stays_inside(&rel, &target) {
                    bail!("symlink {rel_str} -> {target} escapes the SDK");
                }
                symlinks.push((rel, target));
                continue;
            }
            0o100000 => {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut w = std::fs::File::create(&out).with_context(|| format!("creating {}", out.display()))?;
                let copied = std::io::copy(&mut (&mut r).take(filesize), &mut w)?;
                if copied != filesize {
                    bail!("truncated cpio entry {name}");
                }
                #[cfg(unix)]
                if mode & 0o111 != 0 {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
                }
                report.files += 1;
                continue;
            }
            _ => {
                // Devices, fifos: never in an SDK; skip their (empty) data.
                std::io::copy(&mut (&mut r).take(filesize), &mut std::io::sink())?;
                continue;
            }
        }
        if filesize != 0 {
            std::io::copy(&mut (&mut r).take(filesize), &mut std::io::sink())?;
        }
    }
    if !dest.join("SDKSettings.json").is_file() {
        bail!("package has no {marker}/SDKSettings.json; is `sdk` spelled as in the package?");
    }

    // Symlinks last, so a link's target (file or directory) exists when it
    // is created; drop the ones pruning left dangling. Chains (a link to a
    // link) can need a few passes.
    let mut pending = symlinks;
    let real_dest = dest.canonicalize()?;
    loop {
        let before = pending.len();
        let mut next = Vec::new();
        for (rel, target) in pending {
            let link = dest.join(&rel);
            let resolved = link.parent().unwrap_or(dest).join(&target);
            if resolved.exists() {
                // The lexical check at read time can't see through other
                // links (`l1/../..` with `l1` a link); the filesystem can.
                if !resolved.canonicalize()?.starts_with(&real_dest) {
                    bail!("symlink {} -> {target} escapes the SDK", rel.display());
                }
                crate::fs_util::symlink_any(Path::new(&target), &link)?;
                report.symlinks += 1;
            } else {
                next.push((rel, target));
            }
        }
        pending = next;
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }
    report.dangling = pending.len();
    Ok(report)
}

/// `…/SDKs/<sdk>/a/b` → `a/b`; `…/SDKs/<sdk>` → ``; anything else → None.
fn sdk_relative(name: &str, marker: &str) -> Option<PathBuf> {
    let i = name.find(marker)?;
    let rest = &name[i + marker.len()..];
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    let p = Path::new(rest);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(p.to_path_buf())
}

fn is_excluded(patterns: &[glob::Pattern], rel: &str) -> bool {
    let opts = glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };
    // A pattern matching any ancestor directory excludes the whole subtree.
    let mut prefix = String::new();
    for part in rel.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if patterns.iter().any(|p| p.matches_with(&prefix, opts)) {
            return true;
        }
    }
    false
}

/// A relative target that, resolved lexically from the link's directory,
/// stays inside the SDK root.
fn link_stays_inside(rel: &Path, target: &str) -> bool {
    if target.starts_with('/') || target.contains('\\') || target.contains(':') {
        return false;
    }
    let mut depth: i64 = rel.components().count() as i64 - 1;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

#[cfg(test)]
#[path = "tests/apple_sdk.rs"]
mod tests;
