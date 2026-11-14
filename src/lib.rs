//! `eopkg-converter` — convertit un paquet Debian (`.deb`) ou un paquet RPM en
//! paquet Solus (`.eopkg`).
//!
//! Copyright (C) 2026 Julien — licence GNU GPL v3 (voir le fichier LICENSE).
//!
//! Fork de `deb-to-eopkg` étendu au format RPM et doté d'une interface
//! graphique Qt6 (binaire `eopkg-converter-gui`).
//!
//! Le crate est découpé en petits modules :
//!
//! * [`ar`] — lecture de l'archive `ar` (le conteneur d'un `.deb`) ;
//! * [`compress`] — décompression des membres compressés ;
//! * [`payload`] — écriture du chargeur `install.tar.xz` ;
//! * [`eopkg`] — le format eopkg lui-même (XML + ZIP de trois membres) ;
//! * [`zipw`] — un écrivain ZIP minimal (mais valide) ;
//! * [`convert`] — le pilote de conversion `.deb` ;
//! * [`rpm`] — le pilote de conversion `.rpm`.
//!
//! Aucune dépendance à l'exécution : les binaires visés sont statiques
//! (`x86_64-unknown-linux-musl`), ce qui est requis pour installer le paquet
//! depuis une URL sur Solus.
//!
//! NOTE : les binaires graphiques (Qt6) ne peuvent pas être compilés en
//! `musl` dans ce cadre — le `.eopkg` distribué porte le binaire `gnu` qui lie
//! les bibliothèques Qt6, déclarées en dépendances d'exécution.

pub mod ar;
pub mod color;
pub mod compress;
pub mod convert;
pub mod eopkg;
pub mod i18n;
pub mod payload;
pub mod rpm;
pub mod zipw;

/// Nom de l'outil, tel qu'affiché dans les messages.
pub const TOOL_NAME: &str = "eopkg-converter";

/// Version du crate (issue de `Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
