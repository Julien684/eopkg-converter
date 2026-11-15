//! Pilote de conversion `.rpm` -> `.eopkg`.
//!
//! Un `.rpm` est structuré en trois parties :
//!
//! * un **lead** de 96 octets (magic + version) ;
//! * un **en-tête de signature** et un **en-tête de données**, tous deux au
//!   format « header » RPM (magic `0x8eade8`, suivi d'entrées tag/type/offset)
//!   — c'est ici que se trouvent le nom, la version, l'architecture, la
//!   description, et la liste des fichiers (`FILENAME`, `FILESIZE`, `FILEMODE`,
//!   …) ;
//! * la **charge utile** (les fichiers), compressée (xz, gzip, zstd, bzip2 ou
//!   lzma) et contenue dans une archive **cpio newc**.
//!
//! La conversion produit le même `.eopkg` que le pilote `.deb` (même `Summary`,
//! même chargeur `install.tar.xz`) : le type, le mode, l'uid/gid et le hash
//! SHA1 de chaque fichier sont conservés, ainsi que les liens symboliques et
//! physiques.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha1::{Digest, Sha1};

use crate::convert::{human_size, write_package_meta_dir, DebScript, Options, Summary};
use crate::eopkg::{self, Dependency, FileRecord, PackageInfo, TarSource};
use crate::i18n;
use crate::payload::PayloadWriter;
use crate::TOOL_NAME;

/// Niveau de compression xz du chargeur (le défaut de `tarfile`).
const XZ_PRESET: u32 = 6;

/// Magic du lead RPM (4 premiers octets du fichier).
const LEAD_MAGIC: [u8; 4] = [0xed, 0xab, 0xee, 0xdb];
/// Magic d'un en-tête RPM (signature ou données).
const HEADER_MAGIC: u32 = 0x8eade801;
/// Taille du lead.
const LEAD_SIZE: usize = 96;

// ------------------------------------------------------------------ tags RPM
#[allow(dead_code)] // table de référence : tous les tags ci-dessous.
const TAG_NAME: u32 = 1000;
#[allow(dead_code)]
const TAG_VERSION: u32 = 1001;
#[allow(dead_code)]
const TAG_RELEASE: u32 = 1002;
#[allow(dead_code)]
const TAG_SUMMARY: u32 = 1004;
#[allow(dead_code)]
const TAG_DESCRIPTION: u32 = 1005;
#[allow(dead_code)]
const TAG_LICENSE: u32 = 1014;
#[allow(dead_code)]
const TAG_URL: u32 = 1025;
#[allow(dead_code)]
const TAG_ARCH: u32 = 1022;
#[allow(dead_code)]
const TAG_GROUP: u32 = 1003;

#[allow(dead_code)]
const TAG_FILENAME: u32 = 5033;
#[allow(dead_code)]
const TAG_FILESIZE: u32 = 1028;
#[allow(dead_code)]
const TAG_FILEMODE: u32 = 1030;
#[allow(dead_code)]
const TAG_FILEDIGEST: u32 = 1035;
#[allow(dead_code)]
const TAG_FILEDIGESTALGO: u32 = 5011;
#[allow(dead_code)]
const TAG_FILELINKTGT: u32 = 5034;
#[allow(dead_code)]
const TAG_FILELINKTO: u32 = 1036;
#[allow(dead_code)]
const TAG_FILEUID: u32 = 5036;
#[allow(dead_code)]
const TAG_FILEGID: u32 = 5037;
#[allow(dead_code)]
const TAG_FILEDEVMAJOR: u32 = 5038;
#[allow(dead_code)]
const TAG_FILEDEVMINOR: u32 = 5039;

// Scriptlets de maintenance (ordre : pré-installation, post-installation,
// pré-suppression, post-suppression). Stockés en STRING.
const TAG_PREIN: u32 = 1023;
const TAG_POSTIN: u32 = 1024;
const TAG_PREUN: u32 = 1025;
const TAG_POSTUN: u32 = 1026;

// Dépendances d'exécution : noms et versions parallèles.
const TAG_REQUIRENAME: u32 = 1049;
const TAG_REQUIREVERSION: u32 = 1050;

/// Type d'entrée d'un en-tête RPM (le bas 8 bits de l'attribut `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrType {
    Char,
    Int16,
    Int32,
    Int64,
    String,
    Bin,
    StringArray,
    I18nString,
    StringTagSet,
}

impl HdrType {
    fn from_code(code: u8) -> Option<HdrType> {
        Some(match code {
            0x00 => HdrType::Char,
            0x01 => HdrType::Int16,
            0x02 => HdrType::Int32,
            0x03 => HdrType::Int64,
            0x04 => HdrType::String,
            0x05 => HdrType::Bin,
            0x06 => HdrType::StringArray,
            0x07 => HdrType::I18nString,
            0x08 => HdrType::StringTagSet,
            _ => return None,
        })
    }

    /// Types à taille fixe : la taille totale = `count` × taille élémentaire.
    fn is_fixed(self) -> bool {
        matches!(self, HdrType::Char | HdrType::Int16 | HdrType::Int32 | HdrType::Int64)
    }

    /// Taille d'un élément unitaire (0 pour les types à taille variable).
    fn elem_size(self) -> usize {
        match self {
            HdrType::Char => 1,
            HdrType::Int16 => 2,
            HdrType::Int32 => 4,
            HdrType::Int64 => 8,
            _ => 0,
        }
    }
}

/// Un en-tête RPM décodé.
///
/// Pour les types à taille **fixe** (Char, Int16, Int32, Int64), les octets
/// sont stockés directement dans `fixed`. Pour les types à taille **variable**
/// (String, Bin, StringArray, I18nString, StringTagSet), `var` contient
/// `(offset, count)` : l'offset est relatif à `data_area`, et les données sont
/// des chaînes nul-terminées lues à la volée depuis `buf`.
struct Header {
    /// Octets bruts du fichier RPM (pour la lecture différée des types
    /// à taille variable).
    buf: Vec<u8>,
    /// Début absolu de la zone de données (après l'index) dans `buf`.
    data_area: usize,
    /// Types à taille fixe : `tag -> (type, octets)`.
    fixed: BTreeMap<u32, (HdrType, Vec<u8>)>,
    /// Types à taille variable : `tag -> (type, offset relatif, count)`.
    var: BTreeMap<u32, (HdrType, usize, usize)>,
}

impl Header {
    /// Une chaîne (`String`) ou le premier élément d'un `StringArray` /
    /// `I18nString`.
    fn string(&self, tag: u32) -> Option<String> {
        if let Some((_, data)) = self.fixed.get(&tag) {
            return Some(String::from_utf8_lossy(data).into_owned());
        }
        if let Some((_, off, _)) = self.var.get(&tag) {
            let start = self.data_area + off;
            if self.buf.len() < start {
                return None;
            }
            let end = self.buf[start..]
                .iter()
                .position(|&b| b == 0)
                .map(|p| start + p)
                .unwrap_or(self.buf.len());
            return Some(String::from_utf8_lossy(&self.buf[start..end]).into_owned());
        }
        None
    }

    /// Un tableau de chaînes terminées par `0`.
    fn string_array(&self, tag: u32) -> Option<Vec<String>> {
        if let Some((_, off, count)) = self.var.get(&tag) {
            let start = self.data_area + off;
            if self.buf.len() < start {
                return None;
            }
            let mut out = Vec::with_capacity(*count);
            let mut pos = start;
            for _ in 0..*count {
                if pos >= self.buf.len() {
                    break;
                }
                let end = self.buf[pos..]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|p| pos + p)
                    .unwrap_or(self.buf.len());
                out.push(String::from_utf8_lossy(&self.buf[pos..end]).into_owned());
                pos = end + 1;
            }
            return Some(out);
        }
        None
    }

    /// Un entier 32 bits (premier élément du tableau).
    #[allow(dead_code)]
    fn int32(&self, tag: u32) -> Option<i32> {
        let (_, data) = self.fixed.get(&tag)?;
        if data.len() < 4 {
            return None;
        }
        Some(i32::from_be_bytes([data[0], data[1], data[2], data[3]]))
    }

    /// Un entier 64 bits (premier élément du tableau).
    #[allow(dead_code)]
    fn int64(&self, tag: u32) -> Option<i64> {
        let (_, data) = self.fixed.get(&tag)?;
        if data.len() < 8 {
            return None;
        }
        Some(i64::from_be_bytes([
            data[0], data[1], data[2], data[3],
            data[4], data[5], data[6], data[7],
        ]))
    }

    /// Un tableau d'entiers 32 bits (BE).
    #[allow(dead_code)]
    fn int32_array(&self, tag: u32) -> Option<Vec<i64>> {
        let (_, data) = self.fixed.get(&tag)?;
        if data.len() % 4 != 0 {
            return None;
        }
        let mut out = Vec::with_capacity(data.len() / 4);
        let mut i = 0;
        while i + 4 <= data.len() {
            out.push(i32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as i64);
            i += 4;
        }
        Some(out)
    }

    /// Les digests de fichiers (chaînes hexadécimales), alignés sur `FILENAME`.
    fn file_digests(&self) -> Vec<String> {
        self.string_array(TAG_FILEDIGEST).unwrap_or_default()
    }
}

/// Décrit la charge utile compressée d'un `.rpm` (en-tête de données + charge).
struct RpmData {
    /// En-tête de données (nom, version, fichiers…).
    data: Header,
    /// Décalage absolu du début de la charge utile compressée.
    #[allow(dead_code)]
    payload_offset: usize,
    /// Octets de la charge utile compressée.
    payload: Vec<u8>,
}

/// Journalisation minimale sur la sortie d'erreur (comme dans `convert.rs`).
#[derive(Clone)]
struct Log {
    quiet: bool,
    texts: &'static i18n::Texts,
    sink: Option<crate::convert::LogSink>,
}

impl Log {
    fn emit(&self, line: String) {
        if !self.quiet {
            eprintln!("{line}");
        }
        if let Some(sink) = &self.sink {
            sink(&line);
        }
    }

    fn info(&self, msg: impl std::fmt::Display) {
        if !self.quiet || self.sink.is_some() {
            self.emit(format!("{TOOL_NAME} : {msg}"));
        }
    }

    fn warn(&self, msg: impl std::fmt::Display) {
        if !self.quiet || self.sink.is_some() {
            self.emit(format!(
                "{TOOL_NAME} : {}",
                i18n::fill(self.texts.warn_prefix, &[&msg.to_string()])
            ));
        }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Lit un en-tête RPM à partir de `off` (le magic) dans `buf`.
///
/// Renvoie l'en-tête et le décal absolu de sa fin.
///
/// Layout :
/// ```text
/// [magic: 4][reserved: 4][nindex: 4][hsize: 4]  ← 16 octets
/// [index entries: nindex × 16 octets]
/// [data area: chaînes nul-terminées, entiers…]
/// ```
/// Les `offset` dans les entrées d'index sont **relatifs au début de la zone
/// de données**, pas au début de l'en-tête.
fn read_header(buf: &[u8], off: usize) -> io::Result<(Header, usize)> {
    if buf.len() < off + 16 {
        return Err(invalid("en-tête RPM tronqué"));
    }
    let magic = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
    if magic != HEADER_MAGIC {
        return Err(invalid("magic d'en-tête RPM absent (0x8eade8)"));
    }
    let nindex = u32::from_be_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]])
        as usize;
    let hsize = u32::from_be_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]])
        as usize;

    let index_base = off + 16;
    let data_area = index_base + nindex * 16;
    let header_end = off + hsize;

    if buf.len() < header_end {
        return Err(invalid("en-tête RPM tronqué (hsize)"));
    }

    let mut fixed = BTreeMap::new();
    let mut var = BTreeMap::new();

    for i in 0..nindex {
        let e = index_base + i * 16;
        if buf.len() < e + 16 {
            return Err(invalid("section d'index RPM tronquée"));
        }
        let tag = u32::from_be_bytes([buf[e], buf[e + 1], buf[e + 2], buf[e + 3]]);
        let typ = u32::from_be_bytes([buf[e + 4], buf[e + 5], buf[e + 6], buf[e + 7]]);
        let offset = u32::from_be_bytes([buf[e + 8], buf[e + 9], buf[e + 10], buf[e + 11]])
            as usize;
        let count = u32::from_be_bytes([buf[e + 12], buf[e + 13], buf[e + 14], buf[e + 15]])
            as usize;
        let code = (typ & 0xff) as u8;
        let Some(ht) = HdrType::from_code(code) else {
            continue; // type inconnu : on ignore
        };

        // L'offset est relatif au début de la zone de données.
        let data_off = data_area + offset;
        if buf.len() < data_off {
            return Err(invalid("offset de données hors de l'en-tête"));
        }

        if ht.is_fixed() {
            let size = ht.elem_size() * count;
            if buf.len() < data_off + size {
                return Err(invalid("données d'en-tête RPM tronquées"));
            }
            fixed.insert(tag, (ht, buf[data_off..data_off + size].to_vec()));
        } else {
            var.insert(tag, (ht, offset, count));
        }
    }

    Ok((
        Header {
            buf: buf.to_vec(),
            data_area,
            fixed,
            var,
        },
        header_end,
    ))
}

/// Lit et découpe un `.rpm` : lead, en-têtes, charge utile compressée.
///
/// Le lead (96 octets) contient le magic et des champs de version. Les
/// en-têtes de signature et de données sont localisés en scannant le magic
/// `0x8eade801` dans le fichier (leurs offsets ne sont pas stockés dans le
/// lead de manière fiable).
fn split_rpm(buf: &[u8]) -> io::Result<RpmData> {
    if buf.len() < LEAD_SIZE {
        return Err(invalid("fichier trop court pour être un RPM"));
    }
    if buf[..4] != LEAD_MAGIC {
        return Err(invalid("magic de lead RPM absent (fichier non-RPM)"));
    }

    // Signature header : premier magic après le lead.
    let sig_off = find_next_header(buf, LEAD_SIZE)?;
    let (_signature, sig_end) = read_header(buf, sig_off)?;

    // Data header : prochain magic après la fin du signature header.
    let data_off = find_next_header(buf, sig_end)?;
    let (data, _data_end) = read_header(buf, data_off)?;

    // Le `hsize` du data header ne couvre pas toujours toute la zone de
    // données (les chaînes peuvent s'étendre au-delà). On localise le début
    // du payload compressé en scannant le magic de compression à partir de
    // `_data_end`. Si aucun magic n'est trouvé, le payload est non compressé
    // et commence à `_data_end`.
    let payload_offset = find_payload_start(buf, _data_end);
    if buf.len() < payload_offset {
        return Err(invalid("charge utile RPM absente"));
    }
    let payload = buf[payload_offset..].to_vec();

    Ok(RpmData {
        data,
        payload_offset,
        payload,
    })
}

/// Cherche le prochain magic d'en-tête RPM (`0x8eade801`) à partir de `start`.
fn find_next_header(buf: &[u8], start: usize) -> io::Result<usize> {
    let magic = HEADER_MAGIC.to_be_bytes();
    let limit = buf.len().saturating_sub(4);
    let mut i = start;
    while i < limit {
        if buf[i..i + 4] == magic && i > 0 {
            return Ok(i);
        }
        i += 1;
    }
    Err(invalid("en-tête de données RPM introuvable"))
}

/// Localise le début de la charge utile compressée en scannant les magic de
/// compression (gzip, xz, zstd, bzip2, lzma) à partir de `start`.
///
/// Renvoie `start` si aucun magic n'est trouvé (payload non compressé).
fn find_payload_start(buf: &[u8], start: usize) -> usize {
    let limit = buf.len().saturating_sub(6);
    let mut i = start;
    while i < limit {
        if (buf[i] == 0x1f && buf[i + 1] == 0x8b)          // gzip
            || (&buf[i..i + 6] == b"\xfd7zXZ\x00")          // xz
            || (&buf[i..i + 4] == b"\x28\xb5\x2f\xfd")     // zstd
            || (&buf[i..i + 3] == b"BZh")                    // bzip2
            || (buf[i] == 0x5d && buf[i + 1] == 0x00)       // lzma
        {
            return i;
        }
        i += 1;
    }
    // Aucun magic de compression : payload non compressé.
    start
}

/// Décode la charge utile compressée d'après son magic.
fn decode_payload(payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() >= 2 && payload[0] == 0x1f && payload[1] == 0x8b {
        let mut d = flate2::read::MultiGzDecoder::new(payload);
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        Ok(out)
    } else if payload.len() >= 6 && &payload[0..6] == b"\xfd7zXZ\x00" {
        let mut d = xz2::read::XzDecoder::new(payload);
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        Ok(out)
    } else if payload.len() >= 4 && &payload[0..4] == b"\x28\xb5\x2f\xfd" {
        let mut d = zstd::stream::read::Decoder::new(payload)?;
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        Ok(out)
    } else if payload.len() >= 3 && &payload[0..3] == b"BZh" {
        let mut d = bzip2::read::MultiBzDecoder::new(payload);
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        Ok(out)
    } else if payload.len() >= 2 && payload[0] == 0x5d && payload[1] == 0x00 {
        // `.lzma` = flux LZMA seul (« lzma_alone »), sans en-tête xz.
        let stream = xz2::stream::Stream::new_lzma_decoder(u64::MAX)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut d = xz2::read::XzDecoder::new_stream(payload, stream);
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        Ok(out)
    } else {
        // Non compressé (rare) : on le prend tel quel.
        Ok(payload.to_vec())
    }
}

/// Parse une archive `cpio newc` et écrit chaque entrée dans le chargeur.
///
/// Renvoie les `FileRecord` et le total d'octets (fichiers réguliers).
fn extract_cpio<W: Write>(
    cpio: &[u8],
    pw: &mut PayloadWriter<W>,
    log: &Log,
    digests: &[String],
) -> io::Result<(Vec<FileRecord>, u64)> {
    let mut records = Vec::new();
    let mut total: u64 = 0;
    let mut off = 0usize;

    while off + 110 <= cpio.len() {
        // En-tête newc : « 070701 » / « 070702 » (CRC).
        let magic = &cpio[off..off + 6];
        let newc = magic == b"070701" || magic == b"070702";
        if !newc {
            // Marque de fin « TRAILER!!! ».
            if &cpio[off..off + 6] == b"TRAILER" {
                break;
            }
            return Err(invalid("magic cpio newc absent"));
        }
        let f_off = off + 6;
        let in32 = |k: usize| -> u32 {
            let raw = &cpio[f_off + k * 8..f_off + k * 8 + 8];
            let s = std::str::from_utf8(raw).unwrap_or("0");
            u32::from_str_radix(s, 16).unwrap_or(0)
        };
        let inode = in32(0);
        let mode = in32(1);
        let uid = in32(2);
        let gid = in32(3);
        let nlink = in32(4);
        let mtime = in32(5);
        let filesize = in32(6) as usize;
        let devmajor = in32(7);
        let devminor = in32(8);
        let rdevmajor = in32(9);
        let rdevminor = in32(10);
        let namesize = in32(11) as usize;
        let _ = (inode, mtime, devmajor, devminor, rdevmajor, rdevminor);

        let name_off = off + 110;
        if cpio.len() < name_off + namesize {
            return Err(invalid("nom cpio tronqué"));
        }
        let name = String::from_utf8_lossy(&cpio[name_off..name_off + namesize])
            .into_owned();
        // Le `namesize` cpio inclut le byte nul de fin : on le retire.
        let name = name.strip_suffix('\0').unwrap_or(&name).to_string();
        // Alignement 4 du corps (après le nom, qui inclut son `0` de fin).
        let body_off = (name_off + namesize + 3) & !3;
        if cpio.len() < body_off + filesize {
            return Err(invalid("corps cpio tronqué"));
        }
        let body = &cpio[body_off..body_off + filesize];

        // `./` = racine ; `TRAILER!!!` est déjà géré au-dessus.
        if name == "./" {
            off = (body_off + filesize + 3) & !3;
            continue;
        }

        let rel = name.trim_start_matches("./").trim_end_matches('/').to_string();
        if rel.is_empty() {
            off = (body_off + filesize + 3) & !3;
            continue;
        }

        let is_dir = name.ends_with('/');
        let link_mode = mode & 0o170000;

        if is_dir {
            // Les répertoires sont créés à l'extraction par eopkg ; on ne les
            // écrit pas dans le chargeur (comme pour le `.deb`).
        } else if link_mode == 0o120000 {
            // Lien symbolique.
            let target = String::from_utf8_lossy(body).into_owned();
            let target = target.strip_prefix('/').unwrap_or(&target).to_string();
            pw.add_symlink(&rel, mode, &target)?;
            records.push(FileRecord {
                path: rel.clone(),
                ftype: eopkg::classify_type(&rel),
                size: 0,
                uid,
                gid,
                mode,
                hash: String::new(),
            });
        } else if link_mode == 0o100000 && nlink > 1 {
            // Lien physique : dans cpio newc, la « cible » est stockée dans le
            // corps de l'entrée (filesize = longueur du chemin de la cible).
            let target = String::from_utf8_lossy(body).into_owned();
            pw.add_hardlink(&rel, mode, &target)?;
            records.push(FileRecord {
                path: rel.clone(),
                ftype: eopkg::classify_type(&rel),
                size: 0,
                uid,
                gid,
                mode,
                hash: String::new(),
            });
        } else if link_mode == 0o100000 {
            // Fichier régulier.
            // On hache pendant la copie ; le digest RPM (si présent) prime.
            let mut hashing = HashingReader::new(body);
            pw.add_regular(&rel, mode, filesize as u64, &mut hashing)?;
            let hash = hashing.hex();
            records.push(FileRecord {
                path: rel.clone(),
                ftype: eopkg::classify_type(&rel),
                size: filesize as u64,
                uid,
                gid,
                mode,
                hash,
            });
            total += filesize as u64;
        } else {
            log.warn(format!("entrée cpio ignorée ({rel:?})"));
        }

        off = (body_off + filesize + 3) & !3;
    }

    // Si le RPM fournit ses digests, on les substitue (ils sont les
    // « officiels » du paquet, et déjà en hex SHA1 dans la plupart des cas).
    if !digests.is_empty() {
        for (i, r) in records.iter_mut().enumerate() {
            if i < digests.len() && !digests[i].is_empty() && r.hash.is_empty() {
                r.hash = digests[i].clone();
            }
        }
    }

    Ok((records, total))
}

/// Lecteur qui met à jour un SHA1 au fil des octets lus.
struct HashingReader<'a> {
    inner: &'a [u8],
    pos: usize,
    hasher: Sha1,
}

impl<'a> HashingReader<'a> {
    fn new(data: &'a [u8]) -> HashingReader<'a> {
        HashingReader {
            inner: data,
            pos: 0,
            hasher: Sha1::new(),
        }
    }

    fn hex(self) -> String {
        self.hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl Read for HashingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = std::cmp::min(buf.len(), self.inner.len() - self.pos);
        buf[..n].copy_from_slice(&self.inner[self.pos..self.pos + n]);
        self.pos += n;
        self.hasher.update(&buf[..n]);
        if n == 0 {
            Ok(0)
        } else {
            Ok(n)
        }
    }
}

/// Fichier temporaire supprimé à la destruction (comme dans `convert.rs`).
struct Spool {
    path: PathBuf,
}

impl Spool {
    fn new() -> io::Result<Spool> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eopkg-converter-rpm-{}-{nanos}.tar.xz.tmp",
            std::process::id()
        ));
        Ok(Spool { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Chemin de sortie : un répertoire reçoit le nom déduit du paquet.
fn resolve_output(dest: &Path, name: &str, version: &str, arch: &str) -> PathBuf {
    let looks_like_dir = dest.to_string_lossy().ends_with('/') || dest.is_dir();
    if looks_like_dir {
        dest.join(eopkg::eopkg_filename(name, version, "1", arch))
    } else {
        dest.to_path_buf()
    }
}

/// Convertit `rpm` en `.eopkg`.
///
/// `dest` est soit un répertoire (terminé par `/`, ou existant), auquel cas le
/// nom de fichier est déduit du paquet, soit le chemin complet du `.eopkg`.
pub fn convert(rpm: &Path, dest: &Path, opts: Options) -> io::Result<Summary> {
    let texts = opts.lang.texts();
    let log = Log {
        quiet: opts.quiet,
        texts: opts.lang.texts(),
        sink: opts.log_sink.clone(),
    };
    let rpm_size = std::fs::metadata(rpm).map(|m| m.len()).unwrap_or(0);
    log.info(i18n::fill(
        texts.info_source,
        &[&rpm.display().to_string(), &human_size(rpm_size, opts.lang)],
    ));

    let buf = std::fs::read(rpm)?;
    let data = split_rpm(&buf)?;

    // ---- champs de contrôle ----
    let name = data
        .data
        .string(TAG_NAME)
        .ok_or_else(|| invalid("champ Name absent dans le RPM"))?;
    let version = data
        .data
        .string(TAG_VERSION)
        .unwrap_or_else(|| "0.0.0".into());
    let arch = eopkg::eopkg_arch(&data.data.string(TAG_ARCH).unwrap_or_default());

    log.info(i18n::fill(
        texts.info_package,
        &[&name, &version, &arch, &name],
    ));

    // ---- charge utile ----
    let cpio = decode_payload(&data.payload)?;
    let digests = data.data.file_digests();

    let spool = Spool::new()?;
    let spool_file = File::create(spool.path())?;
    let xz = xz2::write::XzEncoder::new(BufWriter::new(spool_file), XZ_PRESET);
    let mut pw = PayloadWriter::new(xz);

    let (records, total) = extract_cpio(&cpio, &mut pw, &log, &digests)?;
    let xz = pw.finish()?;
    let mut buffered = xz.finish()?;
    buffered.flush()?;

    if records.is_empty() {
        return Err(invalid(texts.err_no_files));
    }

    // ---- métadonnées ----
    let mut info = PackageInfo::new(&name, &version, &arch);
    if let Some(s) = non_empty(&data.data, TAG_SUMMARY) {
        info.summary = s;
    }
    if let Some(d) = non_empty(&data.data, TAG_DESCRIPTION) {
        info.description = d;
    }
    if let Some(l) = non_empty(&data.data, TAG_LICENSE) {
        info.license = l;
    }
    if let Some(u) = non_empty(&data.data, TAG_URL) {
        info.homepage = u;
    }
    if let Some(g) = non_empty(&data.data, TAG_GROUP) {
        info.part_of = g.replace('/', ".");
    }
    info.history_comment = "Converted from RPM package".to_string();

    // ---- scripts de maintenance et dépendances (pour information) ----
    let scripts = read_rpm_scripts(&data.data);
    let deps = read_rpm_deps(&data.data);

    let out = resolve_output(dest, &name, &version, &arch);
    log.info(i18n::fill(
        texts.info_generation,
        &[
            &out.display().to_string(),
            &records.len().to_string(),
            &human_size(total, opts.lang),
            &arch,
        ],
    ));

    eopkg::write_eopkg(&out, &info, &records, TarSource::File(spool.path()))?;

    let scripts_dir = write_package_meta_dir(&out, &name, &scripts, &deps)?;

    Ok(Summary {
        out,
        files: records.len() as u64,
        bytes: total,
        arch,
        deps,
        scripts,
        scripts_dir,
    })
}

/// Chaîne non vide d'un tag RPM (beaucoup de RPM déclarent des tags vides).
fn non_empty(hdr: &Header, tag: u32) -> Option<String> {
    hdr.string(tag)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Lit les scriptlets de maintenance d'un RPM (pré/post-installation,
/// pré/post-suppression). Chaque tag contient le script ; l'interpréteur peut
/// être stocké comme premier élément d'un tableau, on ne garde que le corps.
fn read_rpm_scripts(hdr: &Header) -> Vec<DebScript> {
    const STAGES: [(u32, &str); 4] = [
        (TAG_PREIN, "preinst"),
        (TAG_POSTIN, "postinst"),
        (TAG_PREUN, "prerm"),
        (TAG_POSTUN, "postrm"),
    ];
    let mut out = Vec::new();
    for (tag, name) in STAGES {
        let body = match hdr.string_array(tag) {
            Some(parts) if !parts.is_empty() => {
                // Un scriptlet RPM est souvent `[interpreteur, corps]` ; on
                // garde tout sauf un premier élément qui ressemble à un chemin
                // d'interpréteur (`/bin/sh`).
                if parts.len() > 1 && parts[0].starts_with('/') {
                    parts[1..].join("\n")
                } else {
                    parts.join("\n")
                }
            }
            _ => match hdr.string(tag) {
                Some(s) => s,
                None => continue,
            },
        };
        if body.trim().is_empty() {
            continue;
        }
        out.push(DebScript {
            name: name.to_string(),
            content: body.into_bytes(),
        });
    }
    out
}

/// Lit les dépendances `Requires` d'un RPM (`REQUIRENAME` + `REQUIREVERSION`
/// parallèles). Les entrées `rpmlib(...)` et `config(...)` — internes à RPM —
/// sont ignorées.
fn read_rpm_deps(hdr: &Header) -> Vec<Dependency> {
    let names = hdr.string_array(TAG_REQUIRENAME).unwrap_or_default();
    let versions = hdr
        .string_array(TAG_REQUIREVERSION)
        .unwrap_or_default();
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if name.is_empty() || name.starts_with("rpmlib(") || name.starts_with("config(") {
            continue;
        }
        let constraint = versions.get(i).filter(|v| !v.is_empty()).cloned();
        out.push(Dependency {
            name: name.clone(),
            constraint,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit un en-tête RPM minimal (une chaîne `Name`).
    ///
    /// Layout : magic(4) + reserved(4) + nindex(4) + hsize(4) + index(16) + data.
    /// L'offset dans l'index est relatif au début de la zone de données
    /// (qui suit l'index), donc offset=0.
    fn build_header(name: &str) -> Vec<u8> {
        let nindex: u32 = 1;
        // hsize = header(16) + index(16) + données(name.len()).
        let hsize: u32 = 16 + 16 + name.len() as u32;
        let mut v = Vec::new();
        v.extend_from_slice(&HEADER_MAGIC.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&nindex.to_be_bytes());
        v.extend_from_slice(&hsize.to_be_bytes());
        // Entrée : tag=Name, type=String(0x04), offset=0 (relatif à data_area),
        // count=len.
        v.extend_from_slice(&TAG_NAME.to_be_bytes());
        v.extend_from_slice(&0x04u32.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // offset relatif à data_area
        v.extend_from_slice(&(name.len() as u32).to_be_bytes());
        // Données (zone de données)
        v.extend_from_slice(name.as_bytes());
        v
    }

    #[test]
    fn en_tete_minimal() {
        let h = build_header("foo");
        let (hdr, end) = read_header(&h, 0).expect("en-tête invalide");
        assert_eq!(hdr.string(TAG_NAME).as_deref(), Some("foo"));
        assert!(end > 0);
    }

    #[test]
    fn en_tete_string_array() {
        // Construit un header avec un StringArray (type 6) pour NAME.
        let name = "testpkg";
        let nindex: u32 = 1;
        // data_area = 16 + 16 = 32. L'offset=0 pointe vers la zone de données.
        // La zone contient "testpkg\0" (null-terminé).
        let data_len = name.len() + 1; // +1 pour le null
        let hsize: u32 = 16 + 16 + data_len as u32;
        let mut v = Vec::new();
        v.extend_from_slice(&HEADER_MAGIC.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&nindex.to_be_bytes());
        v.extend_from_slice(&hsize.to_be_bytes());
        // Entrée : tag=Name, type=StringArray(0x06), offset=0, count=1
        v.extend_from_slice(&TAG_NAME.to_be_bytes());
        v.extend_from_slice(&0x06u32.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        // Zone de données : "testpkg\0"
        v.extend_from_slice(name.as_bytes());
        v.push(0);

        let (hdr, _) = read_header(&v, 0).expect("en-tête invalide");
        assert_eq!(hdr.string(TAG_NAME).as_deref(), Some("testpkg"));
        let arr = hdr.string_array(TAG_NAME).unwrap();
        assert_eq!(arr, vec!["testpkg".to_string()]);
    }
}
