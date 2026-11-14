//! `pack-eopkg` — construit le `.eopkg` installable de ce projet.
//!
//! Outil de développement : il empaquette les binaires `eopkg-converter` (CLI)
//! et `eopkg-converter-gui` (Qt6), ainsi que le fichier `.desktop`, les icônes,
//! le metainfo AppStream, le README et la licence, dans un paquet Solus.
//!
//! Usage :
//!
//! ```text
//! pack-eopkg [--release N] [dossier_de_sortie]
//! ```
//!
//! Produit `dossier_de_sortie/eopkg-converter-<version>-<release>-1-x86_64.eopkg`.
//!
//! Le *release* doit **strictement augmenter** à chaque publication. C'est le
//! seul champ qui compte pour eopkg : `pisi/operations/upgrade.py` compare
//! `int(release_installe) < int(release_depot)` et ne regarde jamais la chaîne
//! de version (`pisi/db/packagedb.py` lit ce numéro depuis l'attribut
//! `release` de la balise `<Update>` de `<History>`). Republier une version
//! avec un release inchangé rend le paquet invisible à `eopkg up`.
//!
//! Le binaire GUI lie les bibliothèques Qt6 : le paquet déclare donc des
//! dépendances d'exécution (`qt6-qtbase`, …) — il ne peut donc pas être
//! installé « à plat » depuis une URL, seulement depuis un dépôt.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use eopkg_converter::eopkg::{self, Dependency, FileRecord, PackageInfo, TarSource};
use eopkg_converter::payload::PayloadWriter;
use eopkg_converter::TOOL_NAME;

const PACKAGE_NAME: &str = "eopkg-converter";
const PART_OF: &str = "system.utilities";
const PACKAGE_LICENSE: &str = "GPL-3.0";
const HOMEPAGE: &str = "https://depoa.julien68.fr/Linuxon/eopkg-converter";
const PACKAGER_EMAIL: &str = "julien_solus@use.startmail.com";
/// Release par défaut. **À incrémenter à chaque publication** : eopkg ne
/// compare que ce numéro (voir le commentaire en tête de fichier). Surchargeable
/// par `--release N`.
const RELEASE: &str = "1";
// Métadonnées volontairement en ASCII : eopkg affiche les champs via un flux
// encodé ASCII quand la locale n'est pas UTF-8, et lève alors une
// UnicodeEncodeError sur un caractère accentué (vérifié sur Solus 4.9).
const HISTORY_COMMENT: &str = "Fork of deb-to-eopkg: adds RPM support and a Qt6 GUI.";
const SUMMARY: &str = "Convert a Debian .deb or an RPM package into a Solus .eopkg.";
const DESCRIPTION: &str = "\
eopkg-converter is a Rust tool that converts a Debian .deb or an RPM package \
into an installable Solus .eopkg package. It ships two binaries: a command-line \
converter (eopkg-converter) and a Qt6 graphical interface (eopkg-converter-gui). \
The .eopkg produced is a conformant archive (ZIP of metadata.xml, files.xml and \
install.tar.xz) preserving file types, modes and symlinks/hardlinks. Usage: \
eopkg-converter package.deb|package.rpm destination/";

/// Dépendance d'exécution du binaire GUI (Qt6). Le CLI ne dépend de rien.
/// Sur Solus, `qt6-base-gui` fournit QtWidgets, QtGui, QtCore et QDBus.
const RUNTIME_DEPS: [&str; 1] = [
    "qt6-base-gui",
];

/// Documentation embarquée dans l'exécutable de packaging.
const README: &[u8] = include_bytes!("../../README.md");
const LICENSE: &[u8] = include_bytes!("../../LICENSE");

/// Lis un fichier du dépôt (pour le `.desktop`, les icônes, le metainfo).
/// Retourne `None` si le fichier n'existe pas (le pack est donc tolérant).
fn repo_file(name: &str) -> Option<Vec<u8>> {
    std::fs::read(name).ok()
}

/// Date du jour au format ISO `AAAA-MM-JJ` (UTC), sans dépendance externe.
fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    // Algorithme « civil_from_days » (Howard Hinnant).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y0 + 1 } else { y0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usage : pack-eopkg [--release N] [dossier_de_sortie]\n\
             \n\
             Le release doit augmenter à chaque publication, sinon `eopkg up`\n\
             ne verra jamais le paquet (eopkg ne compare que ce numéro)."
        );
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("{TOOL_NAME} pack-eopkg {}", eopkg_converter::VERSION);
        return ExitCode::SUCCESS;
    }

    // `--release N` (ou `--release=N`) : le reste des arguments garde son sens.
    let mut release = RELEASE.to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--release" {
            match iter.next() {
                Some(value) => release = value.clone(),
                None => {
                    eprintln!("pack-eopkg : erreur : --release attend un numéro.");
                    return ExitCode::from(2);
                }
            }
        } else if let Some(value) = arg.strip_prefix("--release=") {
            release = value.to_string();
        } else {
            positional.push(arg.clone());
        }
    }
    if release.parse::<u32>().is_err() {
        eprintln!("pack-eopkg : erreur : release invalide : {release} (un entier attendu).");
        return ExitCode::from(2);
    }

    let out_dir = PathBuf::from(positional.first().map(String::as_str).unwrap_or("dist"));

    match run(&out_dir, &release) {
        Ok(path) => {
            println!("{path}", path = path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("pack-eopkg : erreur : {err}");
            ExitCode::from(1)
        }
    }
}

fn run(out_dir: &Path, release: &str) -> std::io::Result<PathBuf> {
    // Binaires (à fournir par le build avant d'appeler ce pack).
    let cli = std::fs::read("target/release/eopkg-converter")
        .map_err(|e| std::io::Error::new(e.kind(), "binaire CLI manquant : target/release/eopkg-converter (faites cargo build --release)"))?;
    let gui = std::fs::read("target/release/eopkg-converter-gui")
        .map_err(|e| std::io::Error::new(e.kind(), "binaire GUI manquant : target/release/eopkg-converter-gui (faites cargo build --release)"))?;

    // Fichiers de ressources (tolérants : absents → non inclus).
    let desktop = repo_file("eopkg-converter.desktop").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "fichier eopkg-converter.desktop absent")
    })?;
    let metainfo = repo_file("fr.julien68.EopkgConverter.metainfo.xml").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "metainfo fr.julien68.EopkgConverter.metainfo.xml absent")
    })?;
    let icon_24 = repo_file("assets/eopkg-converter-24.png").unwrap_or_default();
    let icon_48 = repo_file("assets/eopkg-converter-48.png").unwrap_or_default();
    let icon_128 = repo_file("assets/eopkg-converter-128.png").unwrap_or_default();
    let screenshot = repo_file("assets/screenshot.png").unwrap_or_default();

    // Le chargeur install.tar.xz : binaires + ressources + documentation.
    let payload: Vec<(&str, Vec<u8>, u32)> = vec![
        ("usr/bin/eopkg-converter", cli, 0o755),
        ("usr/bin/eopkg-converter-gui", gui, 0o755),
        (
            "usr/share/applications/eopkg-converter.desktop",
            desktop,
            0o644,
        ),
        (
            "usr/share/doc/eopkg-converter/README.md",
            README.to_vec(),
            0o644,
        ),
        (
            "usr/share/licenses/eopkg-converter/LICENSE",
            LICENSE.to_vec(),
            0o644,
        ),
        (
            "usr/share/metainfo/fr.julien68.EopkgConverter.metainfo.xml",
            metainfo,
            0o644,
        ),
    ];
    let mut payload = payload;
    if !screenshot.is_empty() {
        payload.push((
            "usr/share/doc/eopkg-converter/assets/screenshot.png",
            screenshot,
            0o644,
        ));
    }
    if !icon_24.is_empty() {
        payload.push((
            "usr/share/icons/hicolor/24x24/apps/eopkg-converter.png",
            icon_24,
            0o644,
        ));
    }
    if !icon_48.is_empty() {
        payload.push((
            "usr/share/icons/hicolor/48x48/apps/eopkg-converter.png",
            icon_48,
            0o644,
        ));
    }
    if !icon_128.is_empty() {
        payload.push((
            "usr/share/icons/hicolor/128x128/apps/eopkg-converter.png",
            icon_128,
            0o644,
        ));
    }

    // Tar xz en mémoire : le paquet complet pèse quelques dizaines de Mo (GUI Qt6).
    let xz = xz2::write::XzEncoder::new(Vec::new(), 6);
    let mut pw = PayloadWriter::new(xz);
    let mut records = Vec::new();
    for (path, data, mode) in &payload {
        pw.add_regular(path, *mode, data.len() as u64, data.as_slice())?;
        records.push(FileRecord {
            path: path.to_string(),
            ftype: eopkg::classify_type(path),
            size: data.len() as u64,
            uid: 0,
            gid: 0,
            mode: *mode,
            hash: sha1_hex(data),
        });
    }
    let tar_xz = pw.finish()?.finish()?;

    let mut info = PackageInfo::new(PACKAGE_NAME, eopkg_converter::VERSION, "x86_64");
    info.release = release.to_string();
    info.summary = SUMMARY.to_string();
    info.description = DESCRIPTION.to_string();
    info.part_of = PART_OF.to_string();
    info.license = PACKAGE_LICENSE.to_string();
    info.homepage = HOMEPAGE.to_string();
    info.packager_email = PACKAGER_EMAIL.to_string();
    info.history_date = today_iso();
    info.history_comment = HISTORY_COMMENT.to_string();
    info.deps = RUNTIME_DEPS
        .iter()
        .map(|d| Dependency {
            name: d.to_string(),
            constraint: None,
        })
        .collect();

    let out = out_dir.join(eopkg::eopkg_filename(
        &info.name,
        &info.version,
        &info.release,
        &info.arch,
    ));
    eopkg::write_eopkg(&out, &info, &records, TarSource::Bytes(&tar_xz))?;
    Ok(out)
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
